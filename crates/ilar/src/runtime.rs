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

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::agent::LoopConfig;
use crate::config::{AgentDefinition, Config, ProjectInstructions, system_prompt_for};
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
    /// `--project-instructions` / `--no-project-instructions`, for one
    /// launch. `None` leaves the decision to configuration.
    pub project_instructions: Option<bool>,
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
    skill_store: Arc<crate::skill::SkillStore>,
    persisted_model: Option<String>,
    cwd: PathBuf,
    questions: bool,
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
    let mut session = store.create(meta).context("creating session")?;
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

pub fn persist_model_change(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    model: &str,
    variant: Option<&str>,
) -> Result<()> {
    drop(resolver.resolve_provider(model)?);
    crate::model::variant_options(model, variant)?;
    let mut session = store.acquire_writer(session_id)?.load()?;
    session.append(crate::session::SessionEvent::ModelChange {
        id: new_id(),
        model: model.to_string(),
        variant: variant.map(String::from),
        ts: chrono::Utc::now(),
    })?;
    Ok(())
}

fn sessions_dir(config: &Config) -> std::path::PathBuf {
    config.state_dir().join("sessions")
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
        let agents = config.agents().context("loading agent definitions")?;
        let agent = agents
            .iter()
            .find(|candidate| candidate.name == agent_name)
            .cloned()
            .with_context(|| format!("unknown agent {agent_name:?}"))?;

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
        crate::model::variant_options(&model, reasoning.as_deref())
            .with_context(|| format!("invalid reasoning for {model}"))?;

        let skill_store = Arc::new(crate::skill::SkillStore::new(
            config.dirs().0.to_path_buf(),
            config.dirs().1.to_path_buf(),
        ));
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
        let commands = crate::command::CommandStore::new(
            config.dirs().0.to_path_buf(),
            config.dirs().1.to_path_buf(),
        )
        .list()
        .context("loading commands")?;

        let project_instructions = selected_project_instructions(
            options.project_instructions,
            config.general.project_instructions,
        );
        let assembled = system_prompt_for(config.dirs().0, &options.cwd, project_instructions)
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
            skill_store,
            persisted_model,
            cwd: options.cwd.clone(),
            questions: options.questions,
            project_instructions,
        })
    }

    /// Create or resume the session and build its tools.
    pub fn start(self, config: &Config) -> Result<SessionRuntime> {
        let store = session_store(config);
        let resolver: Arc<dyn ProviderResolver> = Arc::new(config.clone());
        drop(resolver.resolve_provider(&self.model).with_context(|| {
            format!(
                "no provider configured for {} (set ILAR_ZAI_API_KEY / ILAR_OPENAI_API_KEY)",
                self.model
            )
        })?);

        let session_id = match &self.session_id {
            Some(id) => {
                // A CLI override on a resumed session is a real model
                // change and is recorded as one.
                if self.persisted_model.as_deref() != Some(self.model.as_str()) {
                    persist_model_change(resolver.as_ref(), &store, id, &self.model, None)
                        .with_context(|| format!("persisting model override {}", self.model))?;
                }
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

        let loop_config = LoopConfig {
            compaction_threshold: config.compaction.threshold,
            max_iterations: config.agent.max_iterations,
            ..LoopConfig::default()
        };
        let services = ServiceManager::new();
        let spawner = Arc::new(
            SubagentSpawner::try_new(
                resolver.clone(),
                store.clone(),
                self.agents,
                self.cwd.clone(),
                0,
                config.subagents.max_concurrent,
                config.subagents.max_depth,
                self.project_instructions,
            )?
            .with_user_config_dir(config.dirs().0.to_path_buf())
            // Every published notification also lands here until its
            // delivery is provable from the parent's log, so quitting or
            // crashing with one in flight delays it instead of losing it.
            .with_outbox_dir(config.state_dir().join("outbox"))
            .with_background_tool_timeout(std::time::Duration::from_millis(
                config.subagents.background_tool_timeout_ms,
            ))
            .with_loop_config(loop_config.clone())
            .with_services(services.clone())
            .with_available_models(config.available_models()),
        );
        let todos = Arc::new(Mutex::new(restored_todos(self.resumed.as_ref())));
        let registry = ToolRegistry::builtin()
            .with_subagents(spawner.clone())?
            .with_services(services.clone())?
            .with_models(config.available_models())?
            .with_todos(todos.clone())?
            .with_web_tools()?
            .with_history(store.clone())?
            .with_skills(self.skill_store)?;
        // No receiver, no question tool: a driver that cannot answer
        // makes the call fail immediately rather than hang on it.
        let (registry, questions) = if self.questions {
            let (sender, receiver) = crate::question::question_channel(1);
            (registry.with_questions(sender), Some(receiver))
        } else {
            (registry, None)
        };
        // Oversized bash output is written here, and last week's is
        // swept on the way past. Never fatal: a state directory that
        // cannot be read simply has nothing to clean.
        let spill_dir = crate::tools::bash::spill_dir(config.state_dir());
        crate::tools::bash::clean_spills(&spill_dir);
        // Same errand, same indifference to failure: live-turn scratches
        // whose process died before its drop guard ran.
        crate::session::sweep_live_scratches(&sessions_dir(config));
        // A resumed session's cwd comes off disk and may be gone —
        // deleted worktree, unmounted volume. That is an error to
        // report, not a reason to abort the process.
        let tool_ctx = ToolContext::try_root(self.cwd)?
            .with_subagents(spawner.clone())
            .with_spill_dir(spill_dir);

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
            skills: self.skills,
            commands: self.commands,
            resumed: self.resumed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
