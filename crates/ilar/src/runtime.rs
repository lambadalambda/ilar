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
use crate::config::{AgentDefinition, Config, system_prompt_for};
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
    skill_store: Arc<crate::skill::SkillStore>,
    persisted_model: Option<String>,
    cwd: PathBuf,
    questions: bool,
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

pub fn session_store(config: &Config) -> SessionStore {
    SessionStore::new(config.state_dir().join("sessions"))
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

        let mut system_prompt = system_prompt_for(config.dirs().0, &options.cwd)
            .context("loading project instructions")?;
        if !skill_listing.is_empty() {
            system_prompt = format!("{system_prompt}\n\n{skill_listing}");
        }
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }

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
            skill_store,
            persisted_model,
            cwd: options.cwd.clone(),
            questions: options.questions,
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
                        workspace: None,
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
            SubagentSpawner::new(
                resolver.clone(),
                store.clone(),
                self.agents,
                self.cwd.clone(),
                0,
                config.subagents.max_concurrent,
                config.subagents.max_depth,
            )
            .with_user_config_dir(config.dirs().0.to_path_buf())
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
            .with_skills(self.skill_store)?;
        // No receiver, no question tool: a driver that cannot answer
        // makes the call fail immediately rather than hang on it.
        let (registry, questions) = if self.questions {
            let (sender, receiver) = crate::question::question_channel(1);
            (registry.with_questions(sender), Some(receiver))
        } else {
            (registry, None)
        };
        let tool_ctx = ToolContext::root(self.cwd).with_subagents(spawner.clone());

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
