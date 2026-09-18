//! Everything a driver needs to run turns, built from configuration.
//!
//! The TUI grew this inline: ~180 lines resolving an agent, a model and
//! a reasoning variant, assembling the system prompt, creating or
//! resuming a session, and wiring the spawner, services, todos and tool
//! registry together. None of it is terminal logic, and a second driver
//! that reimplemented it would drift from the first. It lives here so
//! `ilar exec`, the TUI, and anything after them start a session the
//! same way.
//!
//! Two phases on purpose. [`RuntimePlan::resolve`] decides *what* the
//! session will be and touches nothing; [`RuntimePlan::start`] creates
//! or resumes it and builds the tools. `--print-prompt` stops after the
//! first, so asking what the prompt is does not leave an empty session
//! behind.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::agent::LoopConfig;
use crate::config::{AgentDefinition, Config, ProjectInstructions, system_prompt_with};
use crate::provider::ProviderResolver;
use crate::question::QuestionReceiver;
use crate::session::{SessionMeta, SessionReader, SessionStore, new_id};
use crate::subagent::SubagentSpawner;
use crate::todo::TodoList;
use crate::tools::{ToolContext, ToolRegistry, service::ServiceManager};

/// What the caller asks for; every field overrides configuration.
#[derive(Debug, Default, Clone)]
pub struct RuntimeOptions {
    /// `provider/model-id` override for this run.
    pub model: Option<String>,
    /// Agent name from configuration.
    pub agent: Option<String>,
    /// Session to resume; a new one is created when absent.
    pub resume: Option<String>,
    pub cwd: PathBuf,
    /// Attach the `question` tool. A driver with nobody to answer
    /// leaves it off, and the tool call fails instead of hanging.
    pub questions: bool,
    /// Somebody can answer a secret's grant prompt. A driver with
    /// nobody to ask leaves it off, and an ungranted secret is refused
    /// with the CLI line that grants it.
    pub grants: bool,
    /// `--project-instructions` / `--no-project-instructions`, for one
    /// launch. `None` leaves the decision to configuration.
    pub project_instructions: Option<bool>,
    /// The context files read in each location, first found wins;
    /// [`crate::config::CONTEXT_FILES`] when unset. An assistant asks
    /// for [`crate::config::SOUL_FILES`].
    pub context_files: Option<&'static [&'static str]>,
    /// Where the instruction file, skills, agents and commands are read
    /// from; the user config directory when unset. An assistant reads
    /// them from its home.
    pub user_dir: Option<PathBuf>,
    /// Skills from `user_dir` alone: no built-ins, no project
    /// `.ilar/skills`. For an assistant that keeps its own.
    pub own_skills_only: bool,
    /// What this driver's user does about a secret store still sealed
    /// after the prompt had its turn — "/unlock <master password>",
    /// "ilar exec never asks" — put into every refusal the lock
    /// causes. See
    /// [`crate::secrets::Secrets::with_unlock_hint`].
    pub unlock_hint: Option<String>,
    /// Paths no tool call in this session may name. See
    /// [`crate::tools::ToolContext::withheld`]. Empty by default, and
    /// empty for a terminal session, where the person at the keyboard
    /// owns every path the process can reach anyway; a driver that
    /// serves somebody else's session is the one that has to think.
    pub withheld_paths: Vec<PathBuf>,
}

/// The session a driver is about to run, before anything is written.
pub struct RuntimePlan {
    pub session_id: Option<String>,
    pub agent: AgentDefinition,
    pub agents: Vec<AgentDefinition>,
    pub model: String,
    pub reasoning: Option<String>,
    pub system_prompt: String,
    pub skills: Vec<(String, String)>,
    pub commands: Vec<crate::command::Command>,
    pub resumed: Option<SessionReader>,
    /// The name of the working directory's context file when it exists
    /// and this launch left it out; the driver says so rather than
    /// dropping it in silence.
    pub skipped_project_instructions: Option<&'static str>,
    /// What this launch asked for and did not get, one line each: a
    /// reasoning variant the model does not have, an agent override the
    /// session will not remember. A driver shows them; silence here
    /// reads as a bug in the program rather than a rule about the flag.
    pub notices: Vec<String>,
    skill_store: Arc<crate::skill::SkillStore>,
    persisted_model: Option<String>,
    user_dir: PathBuf,
    cwd: PathBuf,
    questions: bool,
    grants: bool,
    unlock_hint: Option<String>,
    withheld_paths: Vec<PathBuf>,
    project_instructions: ProjectInstructions,
}

/// A session, its tools, and the channels a driver listens on.
pub struct SessionRuntime {
    pub store: SessionStore,
    pub session_id: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub agent: AgentDefinition,
    pub system_prompt: String,
    pub registry: ToolRegistry,
    pub spawner: Arc<SubagentSpawner>,
    pub services: Arc<ServiceManager>,
    pub todos: Arc<Mutex<TodoList>>,
    pub tool_ctx: ToolContext,
    pub loop_config: LoopConfig,
    pub resolver: Arc<dyn ProviderResolver>,
    /// `None` when the driver did not ask for questions.
    pub questions: Option<QuestionReceiver>,
    /// `None` when the driver did not offer to answer a tool's asks
    /// for a secret — the grant question and sudo's password question.
    pub grants: Option<crate::secrets::AskReceiver>,
    pub skills: Vec<(String, String)>,
    pub commands: Vec<crate::command::Command>,
    /// The resumed session's replay, for drivers that rebuild a view.
    pub resumed: Option<SessionReader>,
}

/// CLI beats the session's own history, which beats the agent
/// definition, which beats configuration.
pub fn selected_agent_name(cli: Option<&str>, persisted: Option<&str>) -> String {
    cli.or(persisted).unwrap_or("build").to_string()
}

pub fn selected_model(
    cli: Option<&str>,
    persisted: Option<&str>,
    agent: Option<&str>,
    general: &str,
) -> String {
    cli.or(persisted).or(agent).unwrap_or(general).to_string()
}

/// The flag wins over configuration, in both directions: a user who
/// distrusts project files by default still has to be able to opt one
/// in. Both are read at launch and neither is stored on the session —
/// that is the point, since resuming must not smuggle back the project
/// file the current launch refused.
pub fn selected_project_instructions(cli: Option<bool>, configured: bool) -> ProjectInstructions {
    if cli.unwrap_or(configured) {
        ProjectInstructions::Include
    } else {
        ProjectInstructions::Skip
    }
}

/// A resumed session keeps the variant it was running, but only while
/// it keeps its model: a variant means nothing across a model change.
/// New sessions take the configured default.
pub fn selected_reasoning(
    resumed: bool,
    model: &str,
    persisted_model: Option<&str>,
    persisted_reasoning: Option<&str>,
    configured_reasoning: Option<&str>,
) -> Option<String> {
    if resumed {
        (persisted_model == Some(model))
            .then_some(persisted_reasoning)
            .flatten()
            .map(String::from)
    } else {
        configured_reasoning.map(String::from)
    }
}

/// A reasoning variant the model does not have is dropped with a line
/// saying so, not refused: `general.reasoning` is validated once against
/// `general.model` and then applied to every model this launch runs, so
/// a `--model` or an agent's own `model:` would otherwise refuse to
/// start over a default that has nothing to do with it.
pub fn usable_reasoning(
    model: &str,
    reasoning: Option<String>,
) -> (Option<String>, Option<String>) {
    match reasoning {
        Some(variant) if crate::model::variant_options(model, Some(&variant)).is_err() => (
            None,
            Some(format!(
                "{model} has no {variant:?} reasoning variant (general.reasoning): running it without one"
            )),
        ),
        reasoning => (reasoning, None),
    }
}

/// This directory's last session, or none: the pointer's answer, and
/// failing that the listing's — which then repairs the pointer, so the
/// next caller does not pay for the scan again.
///
/// Both surfaces that mean "where I left off" here read this: an empty
/// session the last launch left behind is swept on quit and takes the
/// pointer with it (a pointer may not name a session that is gone), so
/// the listing fallback is not an exotic path — it is what a directory
/// looks like after somebody opened `ilar` and closed it again.
pub fn last_session_here(
    store: &SessionStore,
    cwd: &Path,
) -> Option<crate::session::SessionSummary> {
    // The pointer first: in almost every case this is the answer, and
    // it costs one small JSON read instead of a head read per file in
    // the sessions directory.
    if let Some(session) = store.last_in(cwd) {
        return Some(session);
    }
    let session = store.latest_in(cwd)?;
    store.remember_last(&session.id);
    Some(session)
}

/// The session `--continue` resumes, scoped to where it was typed:
/// this directory's newest session, else the newest anywhere. The
/// second value is the line to say about that fallback — resuming
/// another checkout's conversation against these files is a surprise
/// worth naming — and is `None` when the session is from here.
pub fn latest_session_in(store: &SessionStore, cwd: &Path) -> Result<(String, Option<String>)> {
    if let Some(session) = last_session_here(store, cwd) {
        return Ok((session.id, None));
    }
    let id = latest_session_id(store)?;
    let started_in = store
        .head(&id)
        .ok()
        .and_then(|head| head.meta.cwd)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "a directory it did not record".into());
    Ok((
        id,
        Some(format!(
            "no session started in this directory — continuing the newest one, from {started_in}"
        )),
    ))
}

/// The session `--continue` resumes. An empty store and a store whose
/// sessions all belong to somebody else are different situations: a
/// subagent task's session is resumed through its parent, and a session
/// whose head will not parse is skipped, so "the directory is empty"
/// was a guess in both cases.
pub fn latest_session_id(store: &SessionStore) -> Result<String> {
    if let Some(session) = store.latest() {
        return Ok(session.id);
    }
    let dir = store.root();
    let files = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .count(),
        // A directory that cannot be read is neither of the two
        // situations below, and saying it holds nothing would be a
        // guess about a directory nobody looked in.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "no sessions to continue: reading {} (set ILAR_STATE_DIR to keep sessions elsewhere)",
                    dir.display()
                )
            });
        }
    };
    match files {
        0 => anyhow::bail!("no sessions to continue: {} holds none", dir.display()),
        files => anyhow::bail!(
            "no sessions to continue: none of the {files} session(s) in {} can be resumed on its own (a subagent task's session is resumed through its parent, and an unreadable one is skipped)",
            dir.display()
        ),
    }
}

/// A base system prompt with the agent definition's own prompt hung off
/// it. Every path that runs an agent — the root session here, a
/// foreground or background task, a routed notification — assembles it
/// the same way, so an agent reads identically wherever it is invoked.
pub fn with_agent_prompt(system_prompt: String, agent: &AgentDefinition) -> String {
    if agent.prompt.is_empty() {
        return system_prompt;
    }
    format!(
        "{system_prompt}\n\n# Agent: {}\n\n{}",
        agent.name, agent.prompt
    )
}

/// Child sessions belong to their parent task: resuming one directly
/// would run it outside the workspace lease that governs it.
pub fn ensure_direct_resume_allowed(meta: Option<&SessionMeta>) -> Result<()> {
    if meta.is_some_and(|meta| meta.workspace.is_some()) {
        anyhow::bail!("workspace-bound child sessions must be resumed through Task");
    }
    Ok(())
}

pub fn restored_todos(resumed: Option<&SessionReader>) -> TodoList {
    resumed
        .and_then(SessionReader::todo_list)
        .cloned()
        .unwrap_or_default()
}

/// The directory a new session records as the one it was launched
/// from. Canonicalized because that is what it will be compared
/// against: `WorkspaceLocation` canonicalizes the cwd it carries, so a
/// session started through a symlink must resolve to the same path or
/// it would never look like "here". A directory that cannot be
/// resolved records nothing — a path nothing can be compared against
/// is worse than no path at all.
fn launch_cwd(cwd: &std::path::Path) -> Option<PathBuf> {
    std::fs::canonicalize(cwd).ok()
}

/// Create a root session, recording a non-default reasoning variant
/// before anything can read it. A session that cannot record its
/// variant is removed rather than left behind mislabelled.
pub fn create_root_session(
    store: &SessionStore,
    meta: SessionMeta,
    reasoning: Option<&str>,
) -> Result<()> {
    crate::model::variant_options(&meta.model, reasoning)?;
    let session_id = meta.session_id.clone();
    let model = meta.model.clone();
    let mut session = store.create(meta).with_context(|| {
        format!(
            "creating a session in {} (set ILAR_STATE_DIR to keep sessions elsewhere)",
            store.root().display()
        )
    })?;
    let Some(reasoning) = reasoning else {
        return Ok(());
    };
    let result = session.append(crate::session::SessionEvent::ModelChange {
        id: new_id(),
        model,
        variant: Some(reasoning.to_string()),
        ts: chrono::Utc::now(),
    });
    drop(session);
    if let Err(error) = result {
        let error = anyhow::Error::new(error).context("persisting configured reasoning");
        return match store.delete(&session_id) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "also failed to remove incomplete session {session_id}: {cleanup}"
            ))),
        };
    }
    Ok(())
}

/// Returns the loaded session so a caller that wants to measure the
/// context under the new model can do it on this replay rather than
/// paying a second one.
pub fn persist_model_change(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    model: &str,
    variant: Option<&str>,
) -> Result<crate::session::Session> {
    drop(resolver.resolve_provider(model)?);
    crate::model::variant_options(model, variant)?;
    let mut session = store.acquire_writer(session_id)?.load()?;
    session.append(crate::session::SessionEvent::ModelChange {
        id: new_id(),
        model: model.to_string(),
        variant: variant.map(String::from),
        ts: chrono::Utc::now(),
    })?;
    Ok(session)
}

/// Where every session's log lives — one directory for all of them,
/// which is why a seat that may not read another's has to withhold it
/// by name.
pub fn sessions_dir(config: &Config) -> std::path::PathBuf {
    config.state_dir().join("sessions")
}

pub fn outbox_dir(config: &Config) -> std::path::PathBuf {
    config.state_dir().join("outbox")
}

/// How long an empty session is left alone before the startup sweep
/// takes it. A day, so a launch still sitting at a blank prompt in
/// another terminal is not swept out from under itself.
const EMPTY_SESSION_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// What a runtime does on its way out (the TUI's quit and its session
/// switches, the end of `ilar exec`): a root session nobody said
/// anything in leaves nothing behind, and one that survives becomes its
/// directory's answer to `--continue`. Best-effort — an exit is no
/// place to raise a housekeeping error.
pub fn end_session(config: &Config, store: &SessionStore, session_id: &str) {
    if !store.remove_if_empty(session_id, &outbox_dir(config)) {
        store.remember_last(session_id);
    }
}

pub fn session_store(config: &Config) -> SessionStore {
    SessionStore::new(sessions_dir(config))
}

impl RuntimePlan {
    /// Decide what this session is: which agent, model and reasoning,
    /// and the system prompt they imply. Writes nothing.
    pub fn resolve(config: &Config, options: &RuntimeOptions) -> Result<Self> {
        let store = session_store(config);
        let resumed = options
            .resume
            .as_deref()
            .map(|id| {
                store
                    .load(id)
                    .with_context(|| format!("resuming session {id}"))
            })
            .transpose()?;
        ensure_direct_resume_allowed(resumed.as_ref().and_then(|session| session.meta()))?;

        let persisted_agent = resumed
            .as_ref()
            .and_then(|session| session.meta())
            .map(|meta| meta.agent.clone());
        let agent_name = selected_agent_name(options.agent.as_deref(), persisted_agent.as_deref());
        let user_dir = options
            .user_dir
            .clone()
            .unwrap_or_else(|| config.dirs().0.to_path_buf());
        let agents = config
            .agents_from(&user_dir)
            .context("loading agent definitions")?;
        let agent = agents
            .iter()
            .find(|candidate| candidate.name == agent_name)
            .cloned()
            .ok_or_else(|| {
                let mut known = agents
                    .iter()
                    .map(|agent| agent.name.as_str())
                    .collect::<Vec<_>>();
                known.sort_unstable();
                anyhow::anyhow!(
                    "unknown agent {agent_name:?}; known agents: {}",
                    known.join(", ")
                )
            })?;
        let mut notices = Vec::new();
        // The session records the agent it was created with and nothing
        // records a change, so a `--agent` on a resumed session is this
        // launch only. Better said out loud than discovered on the next
        // `--continue`.
        if let (Some(cli), Some(persisted)) = (options.agent.as_deref(), persisted_agent.as_deref())
            && cli != persisted
        {
            notices.push(format!(
                "agent {cli:?} applies to this launch only: the session is recorded as {persisted:?} and --continue will use that again"
            ));
        }

        let persisted_model = resumed.as_ref().map(|session| session.effective_model());
        let persisted_variant = resumed
            .as_ref()
            .and_then(|session| session.effective_variant());
        let model = selected_model(
            options.model.as_deref(),
            persisted_model.as_deref(),
            agent.model.as_deref(),
            &config.general.model,
        );
        let reasoning = selected_reasoning(
            resumed.is_some(),
            &model,
            persisted_model.as_deref(),
            persisted_variant.as_deref(),
            config.general.reasoning.as_deref(),
        );
        // Before anything is built: an id nothing knows is not a
        // session to plan, and the variant check below would otherwise
        // be the only thing that noticed.
        config.ensure_model_known(&model)?;
        let (reasoning, dropped_reasoning) = usable_reasoning(&model, reasoning);
        notices.extend(dropped_reasoning);

        let skill_store = Arc::new(if options.own_skills_only {
            crate::skill::SkillStore::own_only(user_dir.clone())
        } else {
            crate::skill::SkillStore::new(user_dir.clone(), config.dirs().1.to_path_buf())
        });
        let skill_listing = skill_store
            .listing_prompt()
            .context("loading skill definitions")?;
        let skills = skill_store
            .list()
            .context("loading skill definitions")?
            .into_iter()
            .map(|skill| (skill.name, skill.description))
            .collect();
        // Commands are never listed in the system prompt: unlike skills
        // they are only ever invoked by the user.
        let commands =
            crate::command::CommandStore::new(user_dir.clone(), config.dirs().1.to_path_buf())
                .list()
                .context("loading commands")?;

        let project_instructions = selected_project_instructions(
            options.project_instructions,
            config.general.project_instructions,
        );
        let assembled = system_prompt_with(
            &user_dir,
            &options.cwd,
            project_instructions,
            options
                .context_files
                .unwrap_or(crate::config::CONTEXT_FILES),
        )
        .context("loading project instructions")?;
        let skipped_project_instructions = assembled.skipped_project_file;
        let mut system_prompt = assembled.prompt;
        if !skill_listing.is_empty() {
            system_prompt = format!("{system_prompt}\n\n{skill_listing}");
        }
        let system_prompt = with_agent_prompt(system_prompt, &agent);

        Ok(Self {
            session_id: options.resume.clone(),
            agent,
            agents,
            model,
            reasoning,
            system_prompt,
            skills,
            commands,
            resumed,
            skipped_project_instructions,
            notices,
            skill_store,
            persisted_model,
            user_dir,
            cwd: options.cwd.clone(),
            questions: options.questions,
            grants: options.grants,
            unlock_hint: options.unlock_hint.clone(),
            withheld_paths: options.withheld_paths.clone(),
            project_instructions,
        })
    }

    /// Create or resume the session and build its tools.
    pub fn start(self, config: &Config) -> Result<SessionRuntime> {
        let resolver: Arc<dyn ProviderResolver> = Arc::new(config.clone());
        self.start_with(config, resolver)
    }

    /// [`start`](Self::start) with the provider resolver supplied: what
    /// a driver under test hands a mock, and what a driver with its own
    /// routing hands whatever it routes through.
    pub fn start_with(
        self,
        config: &Config,
        resolver: Arc<dyn ProviderResolver>,
    ) -> Result<SessionRuntime> {
        let store = session_store(config);
        // The resolver's own message names the provider this model
        // needs and what to set, so nothing wraps it: the wrapper
        // printed the same fact twice, its copy naming every key
        // variable including the one already exported. The one thing
        // the resolver cannot know is that a key kept in a sealed store
        // is unreadable while the store is locked — without that, the
        // person who put it there reads this as "the key is gone".
        drop(resolver.resolve_provider(&self.model).map_err(|error| {
            if crate::secrets::SecretStore::open(config.state_dir()).is_locked() {
                error.context("a key kept in the secret store is unreadable while it is locked")
            } else {
                error
            }
        })?);

        let session_id = match &self.session_id {
            Some(id) => {
                // A CLI override on a resumed session is a real model
                // change and is recorded as one.
                if self.persisted_model.as_deref() != Some(self.model.as_str()) {
                    persist_model_change(resolver.as_ref(), &store, id, &self.model, None)
                        .with_context(|| format!("persisting model override {}", self.model))?;
                }
                // Resuming is using: the session's own directory now
                // answers `--continue` with it.
                store.remember_last(id);
                id.clone()
            }
            None => {
                let id = new_id();
                create_root_session(
                    &store,
                    SessionMeta {
                        session_id: id.clone(),
                        parent_id: None,
                        agent: self.agent.name.clone(),
                        model: self.model.clone(),
                        // Not `workspace`: that one means "this session
                        // is a workspace-bound child" and would make the
                        // session unresumable on its own.
                        workspace: None,
                        cwd: launch_cwd(&self.cwd),
                    },
                    self.reasoning.as_deref(),
                )?;
                id
            }
        };

        // Oversized bash output is written here, and last week's is
        // swept on the way past. Never fatal: a state directory that
        // cannot be read simply has nothing to clean.
        crate::tools::bash::clean_spills(&crate::tools::bash::spill_dir(config.state_dir()));
        // Same errand, same indifference to failure: live-turn scratches
        // whose process died before its drop guard ran, the writer locks
        // nobody holds any more, and the sessions a launch created and
        // nobody ever typed into.
        crate::session::sweep_live_scratches(&sessions_dir(config));
        crate::session::sweep_stale_locks(&sessions_dir(config));
        store.sweep_empty_sessions(&outbox_dir(config), EMPTY_SESSION_RETENTION);
        let Tooling {
            registry,
            spawner,
            services,
            todos,
            tool_ctx,
            loop_config,
            questions,
            grants,
        } = self.tooling(config, resolver.clone(), &store)?;

        Ok(SessionRuntime {
            store,
            session_id,
            model: self.model,
            reasoning: self.reasoning,
            agent: self.agent,
            system_prompt: self.system_prompt,
            registry,
            spawner,
            services,
            todos,
            tool_ctx,
            loop_config,
            resolver,
            questions,
            grants,
            skills: self.skills,
            commands: self.commands,
            resumed: self.resumed,
        })
    }

    /// What the session would send, without a session: the model and
    /// its request options, the prompt, and every tool with its
    /// description and schema, built by the same code `start` uses.
    /// The registry is public so a driver can add its own tools
    /// before rendering.
    pub fn preview(&self, config: &Config) -> Result<Preview> {
        let resolver: Arc<dyn ProviderResolver> = Arc::new(config.clone());
        let tooling = self.tooling(config, resolver, &session_store(config))?;
        Ok(Preview {
            model: self.model.clone(),
            reasoning: self.reasoning.clone(),
            options: crate::model::variant_options(&self.model, self.reasoning.as_deref())?,
            agent: self.agent.name.clone(),
            system_prompt: self.system_prompt.clone(),
            registry: tooling.registry,
        })
    }

    /// The tools and their surroundings, exactly as a session gets
    /// them. Nothing here touches the store or the state directory.
    fn tooling(
        &self,
        config: &Config,
        resolver: Arc<dyn ProviderResolver>,
        store: &SessionStore,
    ) -> Result<Tooling> {
        let loop_config = LoopConfig {
            compaction_threshold: config.compaction.threshold,
            max_iterations: config.agent.max_iterations,
            max_output_tokens: (config.agent.max_output_tokens > 0)
                .then_some(config.agent.max_output_tokens),
            ..LoopConfig::default()
        };
        let services = ServiceManager::new();
        // Always attached, even to an empty store: a child shell is
        // shielded from ilar's own keys either way. Only the prompts
        // depend on the driver.
        let secrets =
            crate::secrets::Secrets::new(crate::secrets::SecretStore::open(config.state_dir()));
        // How this driver's user opens a sealed store, for the refusals
        // a locked one causes: the core has no idea which driver it is
        // talking to.
        let secrets = match &self.unlock_hint {
            Some(hint) => secrets.with_unlock_hint(hint.clone()),
            None => secrets,
        };
        // Whether `root` is something the model can ask for at all: the
        // listing says so only where the sudo tool exists.
        let secrets = secrets.with_sudo(config.agent.sudo);
        let (secrets, grants) = if self.grants {
            let (sender, receiver) = crate::secrets::ask_channel(1);
            (secrets.with_prompts(sender), Some(receiver))
        } else {
            (secrets, None)
        };
        // One list for the session and everything it delegates to.
        let withheld: Arc<[PathBuf]> = Arc::from(self.withheld_paths.as_slice());
        let spawner = Arc::new(
            SubagentSpawner::try_new(
                resolver,
                store.clone(),
                self.agents.clone(),
                self.cwd.clone(),
                0,
                config.subagents.max_concurrent,
                config.subagents.max_depth,
                self.project_instructions,
            )?
            .with_user_config_dir(self.user_dir.clone())
            // Every published notification also lands here until its
            // delivery is provable from the parent's log, so quitting or
            // crashing with one in flight delays it instead of losing it.
            .with_outbox_dir(outbox_dir(config))
            .with_background_tool_timeout(std::time::Duration::from_millis(
                config.subagents.background_tool_timeout_ms,
            ))
            .with_loop_config(loop_config.clone())
            .with_services(services.clone())
            .with_available_models(config.available_models())
            .with_secrets(secrets.clone())
            .with_sudo(config.agent.sudo)
            .with_withheld(withheld.clone()),
        );
        let todos = Arc::new(Mutex::new(restored_todos(self.resumed.as_ref())));
        let registry = ToolRegistry::builtin()
            .with_subagents(spawner.clone())?
            .with_services(services.clone())?
            .with_models(config.available_models())?
            .with_todos(todos.clone())?
            .with_web_tools()?
            .with_history(store.clone())?
            .with_skills(self.skill_store.clone())?;
        let registry = match image_gen_backend(config) {
            Some(backend) => registry.with_image_gen(backend)?,
            None => registry,
        };
        // The listing costs a tool; a machine that has never stored a
        // secret does not pay it. A store file that exists does install
        // it, empty or not: the same rule the children use
        // (`SubagentSpawner::agent_registry`), and one `ilar secret set`
        // mid-session no longer leaves the parent without the tool its
        // bash schema points at.
        let registry = if secrets.store().exists() {
            registry.with_secrets()?
        } else {
            registry
        };
        let registry = if config.agent.sudo {
            registry.with_sudo()?
        } else {
            registry
        };
        // No receiver, no question tool: a driver that cannot answer
        // makes the call fail immediately rather than hang on it.
        let (registry, questions) = if self.questions {
            let (sender, receiver) = crate::question::question_channel(1);
            (registry.with_questions(sender), Some(receiver))
        } else {
            (registry, None)
        };
        // A resumed session's cwd comes off disk and may be gone —
        // deleted worktree, unmounted volume. That is an error to
        // report, not a reason to abort the process.
        let tool_ctx = ToolContext::try_root(self.cwd.clone())?
            .with_subagents(spawner.clone())
            .with_spill_dir(crate::tools::bash::spill_dir(config.state_dir()))
            .with_secrets(secrets)
            .with_withheld(withheld);
        Ok(Tooling {
            registry,
            spawner,
            services,
            todos,
            tool_ctx,
            loop_config,
            questions,
            grants,
        })
    }
}

/// The parts of a runtime that are not the session.
struct Tooling {
    registry: ToolRegistry,
    spawner: Arc<SubagentSpawner>,
    services: Arc<ServiceManager>,
    todos: Arc<Mutex<TodoList>>,
    tool_ctx: ToolContext,
    loop_config: LoopConfig,
    questions: Option<QuestionReceiver>,
    grants: Option<crate::secrets::AskReceiver>,
}

/// What the first request of a session would carry, for reading.
pub struct Preview {
    pub model: String,
    pub reasoning: Option<String>,
    /// The provider options the reasoning variant adds; `Null` for none.
    pub options: serde_json::Value,
    pub agent: String,
    pub system_prompt: String,
    pub registry: ToolRegistry,
}

impl Preview {
    /// The whole request as text: a header, the system prompt as sent,
    /// then each tool with its description and input schema.
    pub fn render(&self) -> String {
        let mut out = format!("model: {}\n", self.model);
        if let Some(reasoning) = &self.reasoning {
            out.push_str(&format!("reasoning: {reasoning}\n"));
        }
        if !self.options.is_null() {
            out.push_str(&format!("request options: {}\n", self.options));
        }
        out.push_str(&format!("agent: {}\n", self.agent));
        let tools = self.registry.definitions();
        out.push_str(&format!("tools: {}\n", tools.len()));
        out.push_str("\n===== system prompt =====\n\n");
        out.push_str(&self.system_prompt);
        out.push_str(&format!("\n\n===== tools ({}) =====\n", tools.len()));
        for tool in tools {
            out.push_str(&format!("\n--- {}\n{}\n", tool.name, tool.description));
            let schema = serde_json::to_string_pretty(&tool.input_schema)
                .unwrap_or_else(|_| tool.input_schema.to_string());
            out.push_str(&schema);
            out.push('\n');
        }
        out
    }
}

/// Image generation rides the openai provider's own credentials: the
/// ChatGPT login when that is the configured auth, the API key
/// otherwise, nothing when neither is set or when the provider says
/// `image_gen = false`. Images land under the state
/// directory beside sessions and spills.
fn image_gen_backend(config: &Config) -> Option<crate::tools::image_gen::ImageGenBackend> {
    let settings = config.providers.get("openai")?;
    if !settings.image_gen {
        return None;
    }
    let images_dir = config.state_dir().join("images");
    if settings.auth.as_deref() == Some("chatgpt") {
        return Some(crate::tools::image_gen::ImageGenBackend::with_chatgpt_auth(
            crate::auth::AuthStore::open(config.state_dir().to_path_buf()),
            settings.base_url.clone(),
            images_dir,
        ));
    }
    settings.api_key.clone().map(|key| {
        crate::tools::image_gen::ImageGenBackend::with_api_key(
            key,
            settings.base_url.clone(),
            images_dir,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `general.reasoning` is checked against `general.model` once and
    /// then applied to whatever this launch runs, so a `--model` or an
    /// agent's own model must not be refused over it.
    #[test]
    fn a_variant_the_model_lacks_is_dropped_with_a_line_not_refused() {
        assert_eq!(
            usable_reasoning("zai/glm-4.7", None),
            (None, None),
            "no variant, nothing to say"
        );
        let (kept, notice) = usable_reasoning("openai/gpt-5.6", Some("high".into()));
        assert_eq!(kept.as_deref(), Some("high"));
        assert_eq!(notice, None);

        let (dropped, notice) = usable_reasoning("zai/glm-4.7", Some("high".into()));
        assert_eq!(dropped, None, "an unsupported variant is not sent");
        let notice = notice.expect("dropping a variant is announced");
        assert!(notice.contains("zai/glm-4.7"), "{notice}");
        assert!(notice.contains("\"high\""), "{notice}");
        assert!(notice.contains("general.reasoning"), "{notice}");
    }

    /// "the session directory is empty" was printed for an empty store
    /// and for a store full of sessions that simply cannot be resumed
    /// on their own. Two situations, two next steps.
    #[test]
    fn nothing_to_continue_says_which_kind_of_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let error = latest_session_id(&store).unwrap_err().to_string();
        assert!(error.contains("holds none"), "{error}");

        // A workspace-bound child: listed nowhere, resumable only
        // through its parent.
        let child = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: child.clone(),
                    parent_id: Some(new_id()),
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: None,
                })
                .unwrap(),
        );
        let error = latest_session_id(&store).unwrap_err().to_string();
        assert!(error.contains("1 session(s)"), "{error}");
        assert!(error.contains("resumed through its parent"), "{error}");

        // A session of one's own is found again.
        let own = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: own.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: None,
                })
                .unwrap(),
        );
        assert_eq!(latest_session_id(&store).unwrap(), own);
    }

    /// `--continue` means this directory's work. Opening another
    /// checkout's conversation against these files is a surprise, so
    /// the fallback happens but says where that session started.
    #[test]
    fn continuing_prefers_this_directory_and_names_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let canonical = |dir: &tempfile::TempDir| std::fs::canonicalize(dir.path()).unwrap();

        let theirs = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: theirs.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: Some(canonical(&elsewhere)),
                })
                .unwrap(),
        );

        let (id, notice) = latest_session_in(&store, here.path()).unwrap();
        assert_eq!(id, theirs, "the only session there is");
        let notice = notice.expect("a session from elsewhere is announced");
        assert!(
            notice.contains("no session started in this directory"),
            "{notice}"
        );
        assert!(
            notice.contains(&canonical(&elsewhere).display().to_string()),
            "{notice}"
        );

        // One from here outranks it, however much newer the other is.
        let mine = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: mine.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: Some(canonical(&here)),
                })
                .unwrap(),
        );
        assert_eq!(
            latest_session_in(&store, here.path()).unwrap(),
            (mine, None)
        );
    }

    /// A session file written by hand, so the pointer does not know
    /// about it: the only way to tell "read the pointer" apart from
    /// "read the directory" from the outside.
    fn plant_session(store: &SessionStore, cwd: &Path) -> String {
        let id = new_id();
        let line = serde_json::to_string(&crate::session::SessionEvent::Meta {
            meta: SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: Some(cwd.to_path_buf()),
            },
            ts: chrono::Utc::now(),
        })
        .unwrap();
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(store.session_path(&id).unwrap(), format!("{line}\n")).unwrap();
        id
    }

    /// The pointer is the whole point: `--continue` answers from it
    /// without reading the directory, which is observable because a
    /// session planted behind its back — newer, and from this very
    /// directory — does not win.
    #[test]
    fn continuing_reads_the_pointer_before_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let here = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(here.path()).unwrap();

        let pointed = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: pointed.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: Some(canonical.clone()),
                })
                .unwrap(),
        );
        let newer = plant_session(&store, &canonical);
        assert_eq!(
            store.latest_in(&canonical).map(|session| session.id),
            Some(newer.clone()),
            "the planted session is the newest one the listing sees"
        );

        assert_eq!(
            latest_session_in(&store, here.path()).unwrap(),
            (pointed.clone(), None),
            "the directory was listed instead of the pointer read"
        );

        // A pointer that no longer names a session falls back to the
        // listing — and repairs itself, so the next call is cheap again.
        store.delete(&pointed).unwrap();
        assert_eq!(
            latest_session_in(&store, here.path()).unwrap(),
            (newer.clone(), None)
        );
        assert_eq!(
            store.last_in(&canonical).map(|session| session.id),
            Some(newer),
            "the fallback did not repair the pointer"
        );
    }

    /// The other end of the pointer's life: a runtime that ends leaves
    /// its directory naming the session it was in — unless that session
    /// is one nobody said anything in, which goes instead, pointer and
    /// all.
    #[test]
    fn a_runtime_that_ends_leaves_its_directory_pointed_at_its_session() {
        let guard = tempfile::tempdir().unwrap();
        let config = crate::config::Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
            .config_dir(guard.path().join("config"))
            .state_dir(guard.path().join("state"))
            .resolve()
            .unwrap();
        let store = session_store(&config);
        let here = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(here.path()).unwrap();
        let meta = |session_id: &str| SessionMeta {
            session_id: session_id.into(),
            parent_id: None,
            agent: "build".into(),
            model: "zai/glm-4.7".into(),
            workspace: None,
            cwd: Some(canonical.clone()),
        };

        let spoken = new_id();
        let mut session = store.create(meta(&spoken)).unwrap();
        session
            .append(crate::session::SessionEvent::UserMessage {
                id: new_id(),
                text: "do the thing".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        // A second launch in the same directory, closed without a word:
        // it holds the pointer until its runtime ends.
        let untouched = new_id();
        drop(store.create(meta(&untouched)).unwrap());
        assert_eq!(
            store.last_in(&canonical).map(|session| session.id),
            Some(untouched.clone())
        );

        end_session(&config, &store, &untouched);
        assert!(!store.session_path(&untouched).unwrap().exists());
        assert!(
            store.last_in(&canonical).is_none(),
            "the directory still points at the session that was removed"
        );

        end_session(&config, &store, &spoken);
        assert_eq!(
            store.last_in(&canonical).map(|session| session.id),
            Some(spoken)
        );
    }

    /// A name the program does not know, answered with the names it
    /// does: an agent list is three words long and was left out.
    #[test]
    fn an_unknown_agent_names_the_agents_that_exist() {
        let guard = tempfile::tempdir().unwrap();
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let config = crate::config::Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
            .config_dir(guard.path().join("config"))
            .state_dir(guard.path().join("state"))
            .resolve()
            .unwrap();
        let error = RuntimePlan::resolve(
            &config,
            &RuntimeOptions {
                cwd,
                agent: Some("explorer".into()),
                ..RuntimeOptions::default()
            },
        )
        .map(|_| ())
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown agent \"explorer\""), "{error}");
        assert!(error.contains("build, explore"), "{error}");
    }

    /// Nothing records an agent change, so `--agent` on a resumed
    /// session lasts exactly one launch. The next `--continue` reverted
    /// to the recorded agent without a word.
    #[test]
    fn an_agent_override_on_a_resumed_session_says_it_is_not_recorded() {
        let guard = tempfile::tempdir().unwrap();
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let config = crate::config::Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
            .config_dir(guard.path().join("config"))
            .state_dir(guard.path().join("state"))
            .resolve()
            .unwrap();
        let store = session_store(&config);
        let id = new_id();
        create_root_session(
            &store,
            SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            },
            None,
        )
        .unwrap();
        let plan = |agent: Option<&str>| {
            RuntimePlan::resolve(
                &config,
                &RuntimeOptions {
                    cwd: cwd.clone(),
                    agent: agent.map(str::to_string),
                    resume: Some(id.clone()),
                    ..RuntimeOptions::default()
                },
            )
            .unwrap()
            .notices
        };
        let notices = plan(Some("explore"));
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("this launch only"), "{notices:?}");
        assert!(notices[0].contains("\"build\""), "{notices:?}");

        // The recorded agent, asked for again, is not news.
        assert!(plan(Some("build")).is_empty());
        assert!(plan(None).is_empty());
    }

    #[test]
    fn a_preview_renders_the_request_and_creates_no_session() {
        let guard = tempfile::tempdir().unwrap();
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let config = crate::config::Loader::with_env(vec![("ILAR_ZAI_API_KEY", "zk".to_string())])
            .config_dir(guard.path().join("config"))
            .state_dir(guard.path().join("state"))
            .resolve()
            .unwrap();
        let plan = RuntimePlan::resolve(
            &config,
            &RuntimeOptions {
                cwd,
                questions: true,
                ..RuntimeOptions::default()
            },
        )
        .unwrap();
        let text = plan.preview(&config).unwrap().render();
        assert!(text.starts_with("model: zai/glm-4.7\n"), "{text}");
        assert!(text.contains("===== system prompt ====="), "{text}");
        assert!(text.contains("\n--- read\n"), "{text}");
        assert!(text.contains("\"properties\""), "{text}");
        assert!(text.contains("\n--- question\n"), "{text}");
        assert!(session_store(&config).list().is_empty());
    }

    #[test]
    fn image_generation_follows_the_openai_credential_unless_switched_off() {
        let config_with = |section: &str| {
            let guard = tempfile::tempdir().unwrap();
            let user = guard.path().join("config");
            std::fs::create_dir_all(&user).unwrap();
            std::fs::write(user.join("ilar.toml"), section).unwrap();
            let config = crate::config::Loader::no_env()
                .config_dir(user)
                .state_dir(guard.path().join("state"))
                .resolve()
                .unwrap();
            image_gen_backend(&config).is_some()
        };
        assert!(config_with("[providers.openai]\napi_key = \"k\"\n"));
        assert!(config_with(
            "[providers.openai]\napi_key = \"k\"\nimage_gen = true\n"
        ));
        assert!(!config_with(
            "[providers.openai]\napi_key = \"k\"\nimage_gen = false\n"
        ));
        assert!(!config_with("[providers.zai]\napi_key = \"k\"\n"));
    }

    /// Resuming must not smuggle back the project file the current
    /// launch refused: the prompt is rebuilt from configuration, this
    /// launch's flag and the cwd every time, and nothing about the
    /// decision is stored on the session. Pinned rather than arranged —
    /// this is how the two-phase plan already works, and a change that
    /// started caching the prompt on the session would break it.
    #[test]
    fn a_resumed_session_obeys_the_current_launch_not_the_one_that_created_it() {
        let guard = tempfile::tempdir().unwrap();
        let user = guard.path().join("config");
        let cwd = guard.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(user.join("AGENTS.md"), "user rules\n").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "project rules\n").unwrap();

        let config = crate::config::Loader::no_env()
            .config_dir(user)
            .state_dir(guard.path().join("state"))
            .resolve()
            .unwrap();
        let options = |resume: Option<String>, cli: Option<bool>| RuntimeOptions {
            resume,
            cwd: cwd.clone(),
            project_instructions: cli,
            ..RuntimeOptions::default()
        };

        // A session on disk, and a launch that trusts the project file.
        // (Written directly: `start` would need a reachable provider,
        // and resolve is the phase that assembles the prompt anyway.)
        let store = session_store(&config);
        let session_id = new_id();
        store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: config.general.model.clone(),
                workspace: None,
                cwd: Some(cwd.clone()),
            })
            .unwrap();
        let trusting = RuntimePlan::resolve(&config, &options(None, None)).unwrap();
        assert!(trusting.system_prompt.contains("project rules"));
        assert_eq!(trusting.skipped_project_instructions, None);

        // Resumed under the flag: the file is out, the user's own
        // context stays, and the driver is told to say so.
        let resumed =
            RuntimePlan::resolve(&config, &options(Some(session_id.clone()), Some(false)))
                .expect("the session resumes");
        assert!(!resumed.system_prompt.contains("project rules"));
        assert!(resumed.system_prompt.contains("user rules"));
        assert_eq!(resumed.skipped_project_instructions, Some("AGENTS.md"));

        // And resuming again without the flag brings it back: the
        // refusal is a property of the launch, not of the session.
        let again = RuntimePlan::resolve(&config, &options(Some(session_id), None)).unwrap();
        assert!(again.system_prompt.contains("project rules"));
        assert_eq!(again.skipped_project_instructions, None);
    }

    #[test]
    fn the_flag_wins_over_configuration_in_both_directions() {
        use crate::config::ProjectInstructions::{Include, Skip};
        // Nothing on the command line: configuration decides.
        assert_eq!(selected_project_instructions(None, true), Include);
        assert_eq!(selected_project_instructions(None, false), Skip);
        // --no-project-instructions against the permissive default, and
        // --project-instructions against the paranoid one.
        assert_eq!(selected_project_instructions(Some(false), true), Skip);
        assert_eq!(selected_project_instructions(Some(true), false), Include);
    }

    /// The recorded launch directory is compared against a running
    /// ilar's workspace cwd by exact equality, and that one is
    /// canonical — so this one has to be too, or a session started
    /// through a symlinked path would never look like "here". An
    /// unresolvable directory records nothing rather than a path that
    /// cannot be compared.
    #[test]
    fn the_launch_directory_is_recorded_canonically() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let nested = real.join("workspace");
        std::fs::create_dir(&nested).unwrap();
        let link = real.join("link");
        std::os::unix::fs::symlink(&nested, &link).unwrap();

        assert_eq!(launch_cwd(&nested), Some(nested.clone()));
        assert_eq!(launch_cwd(&link), Some(nested));
        assert_eq!(launch_cwd(&real.join("gone")), None);
    }

    #[test]
    fn selection_respects_cli_over_history_over_agent_over_config() {
        assert_eq!(selected_agent_name(None, None), "build");
        assert_eq!(selected_agent_name(None, Some("explore")), "explore");
        assert_eq!(
            selected_agent_name(Some("review"), Some("explore")),
            "review"
        );

        assert_eq!(
            selected_model(None, None, Some("zai/agent-model"), "zai/general"),
            "zai/agent-model"
        );
        assert_eq!(
            selected_model(
                Some("openai/cli"),
                None,
                Some("zai/agent-model"),
                "zai/general"
            ),
            "openai/cli"
        );
        assert_eq!(
            selected_model(
                None,
                Some("openai/persisted"),
                Some("zai/agent-model"),
                "zai/general"
            ),
            "openai/persisted"
        );

        assert_eq!(
            selected_reasoning(false, "openai/gpt-5.2", None, None, Some("high")),
            Some("high".into()),
            "new sessions use configured reasoning"
        );
        assert_eq!(
            selected_reasoning(
                true,
                "openai/gpt-5.2",
                Some("openai/gpt-5.2"),
                Some("low"),
                Some("high")
            ),
            Some("low".into()),
            "resumed sessions preserve their variant"
        );
        assert_eq!(
            selected_reasoning(
                true,
                "openai/gpt-5.2",
                Some("openai/gpt-5.2"),
                None,
                Some("high")
            ),
            None,
            "a resumed server-default variant stays default"
        );
        assert_eq!(
            selected_reasoning(
                true,
                "openai/gpt-5.3-codex",
                Some("openai/gpt-5.2"),
                Some("high"),
                Some("low")
            ),
            None,
            "a resumed session's variant does not leak across a model override"
        );
    }
}
