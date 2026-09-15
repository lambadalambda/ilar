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
    user_dir: PathBuf,
    cwd: PathBuf,
    questions: bool,
    grants: bool,
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
    /// `None` when the driver did not offer to answer grant prompts.
    pub grants: Option<crate::secrets::GrantReceiver>,
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
            skill_store,
            persisted_model,
            user_dir,
            cwd: options.cwd.clone(),
            questions: options.questions,
            grants: options.grants,
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
        drop(resolver.resolve_provider(&self.model).with_context(|| {
            format!(
                "no provider configured for {} (set ILAR_ZAI_API_KEY, ILAR_OPENAI_API_KEY or ILAR_OPENCODE_API_KEY)",
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

        // Oversized bash output is written here, and last week's is
        // swept on the way past. Never fatal: a state directory that
        // cannot be read simply has nothing to clean.
        crate::tools::bash::clean_spills(&crate::tools::bash::spill_dir(config.state_dir()));
        // Same errand, same indifference to failure: live-turn scratches
        // whose process died before its drop guard ran.
        crate::session::sweep_live_scratches(&sessions_dir(config));
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
        let (secrets, grants) = if self.grants {
            let (sender, receiver) = crate::secrets::grant_channel(1);
            (secrets.with_prompts(sender), Some(receiver))
        } else {
            (secrets, None)
        };
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
            .with_outbox_dir(config.state_dir().join("outbox"))
            .with_background_tool_timeout(std::time::Duration::from_millis(
                config.subagents.background_tool_timeout_ms,
            ))
            .with_loop_config(loop_config.clone())
            .with_services(services.clone())
            .with_available_models(config.available_models())
            .with_secrets(secrets.clone()),
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
        // The listing costs a tool; an empty store does not pay it.
        let registry = if secrets.values().is_empty() {
            registry
        } else {
            registry.with_secrets()?
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
            .with_secrets(secrets);
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
    grants: Option<crate::secrets::GrantReceiver>,
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
