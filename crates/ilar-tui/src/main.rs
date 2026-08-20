//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod app;
mod decide;
mod diff;
mod highlight;
mod history;
mod input;
mod markdown;
mod modals;
mod selection;
mod session_view;
mod sidebar;
mod text;
mod theme;
mod transcript;

use std::sync::Arc;

use anyhow::{Context, Result};
use app::{App, activate_palette_command, apply_theme_picker_action};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use decide::{Intent, LoopState, after_turn, may_route_notification, retry as retry_intents};
use input::{InputBuffer, PromptAction, handle_prompt_key, retry_requested};
use modals::{
    CommandPaletteAction, Modal, ModelPicker, PendingAction, PendingManager, PickerAction,
    SessionPicker, SessionPickerAction, ThemePicker, VariantPicker, VariantPickerAction,
    is_command_palette_shortcut,
};
use ratatui::style::Color;
use input::slash_candidates;
use tokio_util::sync::CancellationToken;
use transcript::Line_;

use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEventReceiver, TurnOutcome, loop_event_channel, run_turn,
};
use ilar::config::{Loader, system_prompt_for};
use ilar::provider::ProviderResolver;
use ilar::session::{SessionMeta, SessionStore, new_id};
use ilar::subagent::SubagentSpawner;
use ilar::tools::{ToolContext, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    Ready,
    Thinking,
    Responding,
    Tools,
    Aborting,
    Aborted,
    Stopped,
    Paused,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeLevel {
    Info,
    Warning,
    Error,
}

const MUTED: Color = theme::MUTED;
const ERROR: Color = theme::ERROR;
const MAX_WHEEL_EVENTS_PER_BATCH: usize = 1024;

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Log in to OpenAI with your ChatGPT account (OAuth in the browser)
    Login,
}

#[derive(Parser, Debug)]
#[command(name = "ilar", version, about = "personal coding agent")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Model to use (provider/model-id); overrides config.
    #[arg(long)]
    model: Option<String>,

    /// Session id to resume.
    #[arg(long)]
    session: Option<String>,

    /// Resume the most recently modified session.
    #[arg(long = "continue", conflicts_with = "session")]
    continue_last: bool,

    /// Agent name from config (markdown agents).
    #[arg(long)]
    agent: Option<String>,

    /// Print the resolved system prompt and exit (debugging).
    #[arg(long)]
    print_prompt: bool,
}

struct TerminalSession {
    terminal_initialized: bool,
    keyboard_enhanced: bool,
    mouse_enabled: bool,
    paste_enabled: bool,
}

impl TerminalSession {
    fn start() -> Result<(ratatui::DefaultTerminal, Self)> {
        let mut session = Self {
            terminal_initialized: false,
            keyboard_enhanced: false,
            mouse_enabled: false,
            paste_enabled: false,
        };
        let terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                ratatui::restore();
                return Err(error.into());
            }
        };
        session.terminal_initialized = true;

        if supports_keyboard_enhancement().unwrap_or(false) {
            session.keyboard_enhanced = true;
            crossterm::execute!(
                std::io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
        }

        session.mouse_enabled = true;
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
        session.paste_enabled = true;
        if let Err(error) = crossterm::execute!(std::io::stdout(), EnableBracketedPaste) {
            if error.kind() == std::io::ErrorKind::Unsupported {
                let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
                session.paste_enabled = false;
            } else {
                return Err(error.into());
            }
        }
        Ok((terminal, session))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.paste_enabled {
            let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        }
        if self.mouse_enabled {
            let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        }
        if self.keyboard_enhanced {
            let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.terminal_initialized {
            ratatui::restore();
        }
    }
}

/// Turn what the user typed into what gets sent: arm or edit a goal,
/// expand a command, or route a skill. Returns `None` when the input
/// was consumed without starting a turn.
///
/// Every path that starts a turn goes through this. When only the
/// interactive Enter did, a queued `/goal ship it` was sent to the model
/// as literal text once the turn it was waiting on finished.
fn prepare_prompt(app: &mut App, text: String) -> Option<String> {
    if let Some(("goal", goal_text)) = parse_slash_invocation(&text) {
        if goal_text.is_empty() {
            match &app.goal {
                Some((goal, _)) => {
                    // Prefill for editing; Esc on an emptied input aborts.
                    app.input = InputBuffer::from(format!("/goal {goal}"));
                    app.set_notice(
                        "edit the goal and press Enter — /goal abort ends it",
                        NoticeLevel::Info,
                    );
                }
                None => app.set_notice(
                    "no active goal — /goal <description> sets one",
                    NoticeLevel::Info,
                ),
            }
            return None;
        }
        if goal_text == "abort" {
            let notice = match app.goal.take() {
                Some((goal, round)) => {
                    let message = format!("goal aborted after {round} round(s): {goal}");
                    app.push_transcript_line(Line_::System(message.clone()));
                    message
                }
                None => "no active goal".into(),
            };
            app.set_notice(notice, NoticeLevel::Info);
            return None;
        }
        if let Some((goal, round)) = &mut app.goal {
            // Editing mid-loop keeps the round budget; the next
            // continuation carries the new wording.
            if goal != goal_text {
                *goal = goal_text.to_string();
                let round = *round;
                app.push_transcript_line(Line_::System(format!(
                    "goal updated (round {round}/{MAX_GOAL_ROUNDS}): {goal_text}"
                )));
            }
            app.clear_transient_notice();
            return None;
        }
        app.goal = Some((goal_text.to_string(), 0));
        app.push_transcript_line(Line_::System(format!(
            "goal armed (max {MAX_GOAL_ROUNDS} rounds): {goal_text}"
        )));
        return Some(goal_kickoff_prompt(goal_text));
    }
    if let Some((name, args)) = parse_slash_invocation(&text) {
        match resolve_slash(app, name, args) {
            SlashResolution::Prompt(expanded) => return Some(expanded),
            SlashResolution::Skill(prompt) => return Some(prompt),
            SlashResolution::Empty => {
                app.input = InputBuffer::from(text.as_str());
                app.set_notice(
                    format!("/{name} needs arguments — its body is only placeholders"),
                    NoticeLevel::Warning,
                );
                return None;
            }
            SlashResolution::Unknown(matches) => {
                app.input = InputBuffer::from(text.as_str());
                app.set_notice(
                    format!("unknown /{name} · available: {}", matches.join(", ")),
                    NoticeLevel::Warning,
                );
                return None;
            }
        }
    }
    Some(text)
}

/// Apply one intent to the app. Returns the prompt when the intent
/// starts a turn — spawning needs the runtime, everything else does
/// not, and keeping the split here is what makes the wiring testable.
fn apply_intent(
    app: &mut App,
    intent: Intent,
    steer: Option<&ilar::agent::SteerSender>,
) -> Option<String> {
    match intent {
        Intent::Notice(text, level) => {
            app.set_notice(text, level);
            None
        }
        Intent::SystemLine(text) => {
            app.push_transcript_line(Line_::System(text));
            None
        }
        Intent::ClearGoal => {
            app.goal = None;
            None
        }
        Intent::AdvanceGoal(round) => {
            if let Some((_, current)) = app.goal.as_mut() {
                *current = round;
            }
            None
        }
        Intent::Steer(text) => {
            // The channel can close between the decision and here — the
            // turn ending is exactly when that happens — and the message
            // must not be lost with it.
            match steer {
                Some(tx) if tx.send(text.clone()).is_ok() => {
                    app.pending_steers.push(text);
                    app.set_notice(
                        "steering — reaches the model at the next step",
                        NoticeLevel::Info,
                    );
                    None
                }
                _ => apply_intent(app, Intent::Queue(text), steer),
            }
        }
        Intent::Queue(text) => {
            app.queued_messages.push(text);
            app.set_notice(
                format!(
                    "queued ({} waiting) — sends when the turn completes",
                    app.queued_messages.len()
                ),
                NoticeLevel::Info,
            );
            None
        }
        Intent::PastePalette(text) => {
            if let Some(palette) = app.command_palette.as_mut() {
                palette.insert_query(&text);
            }
            None
        }
        Intent::PasteSearch(text) => {
            app.search_query.push_str(text.trim());
            app.search_refresh();
            None
        }
        Intent::PasteInput(text) => {
            app.model_key_pending = false;
            app.clear_transient_notice();
            app.input.insert(&text);
            None
        }
        Intent::SendQueued => {
            let next = (!app.queued_messages.is_empty()).then(|| app.queued_messages.remove(0))?;
            apply_intent(app, Intent::StartTurn(next), steer)
        }
        Intent::StartTurn(text) => {
            // Recorded before expansion, so a retry replays what the
            // user actually typed.
            app.last_prompt = Some(text.clone());
            let text = prepare_prompt(app, text)?;
            app.retry_available = false;
            app.clear_notice();
            app.push_transcript_line(Line_::User(text.clone()));
            app.follow_tail = true;
            app.busy = true;
            app.status = "thinking".into();
            app.set_activity(Activity::Thinking);
            Some(text)
        }
    }
}

/// Apply event-side intents now, deferring only turn starts to the
/// central drain where spawning lives. Applying immediately matters: a
/// steer deferred by one loop tick can miss the turn it was aimed at,
/// and a queue push deferred past the completion check strands the
/// message instead of auto-sending it.
fn apply_event_intents(
    app: &mut App,
    decided: Vec<Intent>,
    deferred: &mut Vec<Intent>,
    steer: Option<&ilar::agent::SteerSender>,
) {
    for intent in decided {
        match intent {
            Intent::StartTurn(_) => deferred.push(intent),
            other => {
                let started = apply_intent(app, other, steer);
                debug_assert!(started.is_none(), "only StartTurn yields a prompt");
            }
        }
    }
}

/// Rounds after which goal mode gives up (a budget, unlike the
/// runaway-loop iteration guard).
const MAX_GOAL_ROUNDS: u32 = 25;
const GOAL_SENTINEL: &str = "GOAL_ACHIEVED";

/// True when the assistant's final text declares the goal achieved
/// (sentinel at a line start, so prose mentions don't trigger it).
fn goal_achieved_in(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(GOAL_SENTINEL))
}

fn goal_kickoff_prompt(goal: &str) -> String {
    format!(
        "Work toward this goal: {goal}

This is a goal-mode session: after          each of your turns you will be asked to verify progress with          concrete evidence. If no automatic verification exists yet          (tests, a replay harness, a checker script), building one is part          of the goal. Do not claim success without evidence."
    )
}

fn goal_continuation_prompt(goal: &str, round: u32) -> String {
    format!(
        "Goal check, round {round}/{MAX_GOAL_ROUNDS}. The goal: {goal}

         Verify the current state with concrete evidence by running your          verification (tests, harness, checker) now — do not judge from          memory. If the goal is genuinely achieved, output a line starting          with `{GOAL_SENTINEL}:` followed by the evidence. Otherwise state          what is still missing and continue working toward the goal in          this same turn."
    )
}

/// `/name args` parsed from a submitted prompt; `None` when the text is
/// not shaped like a skill invocation (and should submit unchanged).
fn parse_slash_invocation(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
    {
        return None;
    }
    Some((name, args.trim()))
}

fn skill_invocation_prompt(name: &str, args: &str) -> String {
    if args.is_empty() {
        format!("Use the `skill` tool to load the skill \"{name}\" and follow its instructions.")
    } else {
        format!(
            "Use the `skill` tool to load the skill \"{name}\" and follow its instructions. Arguments: {args}"
        )
    }
}

/// Observe the loop, completely. Every field is read from the real
/// source, so a decision cannot be fed a plausible-looking default for
/// something the caller did not bother to compute — which is how a
/// snapshot type quietly becomes a lie.
fn observe(
    app: &App,
    turn_handle: &Option<tokio::task::JoinHandle<TurnCompletion>>,
    pending_terminal_event: &Option<Event>,
    steer_tx: &Option<ilar::agent::SteerSender>,
    notifications_paused: bool,
) -> LoopState {
    LoopState {
        turn_running: turn_handle.is_some(),
        modal: app.active_modal(),
        input_blank: app.input.is_blank(),
        pending_event: pending_terminal_event.is_some(),
        queued: app.queued_messages.len(),
        steerable: steer_tx.as_ref().is_some_and(|tx| !tx.is_closed()),
        notifications_paused,
    }
}

/// What `/name args` means. Extracted from the key handler so the
/// precedence between commands, skills and the built-in is testable
/// without a running event loop.
#[derive(Debug, PartialEq)]
enum SlashResolution {
    /// A command's body, arguments already substituted.
    Prompt(String),
    /// A request for the model to load a skill.
    Skill(String),
    /// Nothing by that name; near matches to suggest.
    Unknown(Vec<String>),
    /// A command whose body expanded to nothing.
    Empty,
}

fn resolve_slash(app: &App, name: &str, args: &str) -> SlashResolution {
    if let Some(command) = app.commands.iter().find(|command| command.name == name) {
        let expanded = ilar::command::expand(&command.template, args);
        // A body of just `$ARGUMENTS` invoked bare expands to nothing,
        // and an empty prompt is rejected by the provider.
        if expanded.trim().is_empty() {
            return SlashResolution::Empty;
        }
        return SlashResolution::Prompt(expanded);
    }
    if app.skills.iter().any(|(skill, _)| skill == name) {
        return SlashResolution::Skill(skill_invocation_prompt(name, args));
    }
    let mut inventory = app.slash_inventory();
    inventory.push(("goal".into(), String::new()));
    SlashResolution::Unknown(close_skill_matches(&inventory, name))
}

fn close_skill_matches(skills: &[(String, String)], name: &str) -> Vec<String> {
    let lowered = name.to_lowercase();
    let mut matches: Vec<String> = skills
        .iter()
        .filter(|(candidate, _)| candidate.to_lowercase().contains(&lowered))
        .map(|(candidate, _)| candidate.clone())
        .collect();
    if matches.is_empty() {
        matches = skills
            .iter()
            .map(|(candidate, _)| candidate.clone())
            .collect();
    }
    matches.truncate(6);
    matches
}

fn selected_agent_name(cli: Option<&str>, persisted: Option<&str>) -> String {
    cli.or(persisted).unwrap_or("build").to_string()
}

fn selected_model(
    cli: Option<&str>,
    persisted: Option<&str>,
    agent: Option<&str>,
    general: &str,
) -> String {
    cli.or(persisted).or(agent).unwrap_or(general).to_string()
}

fn persist_model_change(
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    model: &str,
    variant: Option<&str>,
) -> Result<()> {
    drop(resolver.resolve_provider(model)?);
    ilar::model::variant_options(model, variant)?;
    let mut session = store.acquire_writer(session_id)?.load()?;
    session.append(ilar::session::SessionEvent::ModelChange {
        id: ilar::session::new_id(),
        model: model.to_string(),
        variant: variant.map(String::from),
        ts: chrono::Utc::now(),
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn adopt_model_selection(
    app: &mut App,
    resolver: &dyn ProviderResolver,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    model: String,
    variant: Option<String>,
) -> Result<()> {
    persist_model_change(resolver, store, session_id, &model, variant.as_deref())?;
    app.current_model = model.clone();
    app.current_variant = variant.clone();
    app.context_limit = display_context_limit(resolver, &model);
    if let Ok((used, estimated)) =
        session_context_tokens(store, session_id, system_prompt, registry)
    {
        app.context_used = used;
        app.context_estimated = estimated;
    }
    app.status = "ready".into();
    app.clear_notice();
    let selection = variant
        .as_deref()
        .map(|variant| format!("{model}@{variant}"))
        .unwrap_or(model);
    app.push_transcript_line(Line_::System(format!("switched to {selection}")));
    Ok(())
}

fn ensure_direct_resume_allowed(meta: Option<&SessionMeta>) -> Result<()> {
    if meta.is_some_and(|meta| meta.workspace.is_some()) {
        anyhow::bail!("workspace-bound child sessions must be resumed through Task");
    }
    Ok(())
}

fn restored_todos(resumed: Option<&ilar::session::SessionReader>) -> ilar::todo::TodoList {
    resumed
        .and_then(ilar::session::SessionReader::todo_list)
        .cloned()
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Loader::new().resolve().context("loading config")?;
    if let Some(Command::Login) = args.command {
        let store = ilar::auth::AuthStore::open(config.state_dir().to_path_buf());
        let tokens = ilar::auth::login_flow(&store, std::time::Duration::from_secs(300), true)
            .await
            .context("login failed")?;
        println!(
            "Logged in as ChatGPT account {}",
            tokens
                .account_id
                .as_deref()
                .unwrap_or("(account id unknown)")
        );
        println!("Tokens stored at {}", store.tokens_path().display());
        return Ok(());
    }

    let configured_theme = theme::ThemeId::parse(&config.general.theme).with_context(|| {
        format!(
            "unknown theme {:?}; expected one of: {}",
            config.general.theme,
            theme::ThemeId::ALL
                .iter()
                .map(|theme| theme.id())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let store = SessionStore::new(config.state_dir().join("sessions"));
    // The whole runtime (agent, model, prompt, registry) is rebuilt per
    // session so switching via the picker restarts with full fidelity.
    let mut session_override: Option<String> = None;
    let mut first_run = true;
    let mut terminal_hold: Option<(ratatui::DefaultTerminal, TerminalSession)> = None;
    let mut active_theme = configured_theme;

    loop {
        let resume_target = if first_run {
            if args.continue_last {
                Some(
                    store
                        .latest()
                        .map(|session| session.id)
                        .context("no sessions to continue (session directory is empty)")?,
                )
            } else {
                args.session.clone()
            }
        } else {
            session_override.clone()
        };
        // CLI overrides apply to the launch session only, not picker switches.
        let cli_model = if first_run {
            args.model.as_deref()
        } else {
            None
        };
        let cli_agent = if first_run {
            args.agent.as_deref()
        } else {
            None
        };
        first_run = false;

        let resumed = resume_target
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
        let agent_name = selected_agent_name(cli_agent, persisted_agent.as_deref());
        let agents = config.agents().context("loading agent definitions")?;
        let agent = agents
            .iter()
            .find(|a| a.name == agent_name)
            .cloned()
            .with_context(|| format!("unknown agent {agent_name:?}"))?;
        let persisted_model = resumed.as_ref().map(|session| session.effective_model());
        let persisted_variant = resumed
            .as_ref()
            .and_then(|session| session.effective_variant());
        let model_for_session = selected_model(
            cli_model,
            persisted_model.as_deref(),
            agent.model.as_deref(),
            &config.general.model,
        );

        let cwd = std::env::current_dir().context("no cwd")?;
        let skill_store = std::sync::Arc::new(ilar::skill::SkillStore::new(
            config.dirs().0.to_path_buf(),
            config.dirs().1.to_path_buf(),
        ));
        let skill_listing = skill_store
            .listing_prompt()
            .context("loading skill definitions")?;
        let skill_inventory: Vec<(String, String)> = skill_store
            .list()
            .context("loading skill definitions")?
            .into_iter()
            .map(|skill| (skill.name, skill.description))
            .collect();
        // Commands are never listed in the system prompt: unlike skills
        // they are only ever invoked by the user.
        let command_inventory = ilar::command::CommandStore::new(
            config.dirs().0.to_path_buf(),
            config.dirs().1.to_path_buf(),
        )
        .list()
        .context("loading commands")?;
        let mut system_prompt =
            system_prompt_for(config.dirs().0, &cwd).context("loading project instructions")?;
        if !skill_listing.is_empty() {
            system_prompt = format!("{system_prompt}\n\n{skill_listing}");
        }
        if !agent.prompt.is_empty() {
            system_prompt = format!(
                "{system_prompt}\n\n# Agent: {}\n\n{}",
                agent.name, agent.prompt
            );
        }
        if args.print_prompt {
            println!("{system_prompt}");
            return Ok(());
        }

        let resolver: Arc<dyn ProviderResolver> = Arc::new(config.clone());
        drop(
        resolver
            .resolve_provider(&model_for_session)
            .with_context(|| format!("no provider configured for {model_for_session} (set ILAR_ZAI_API_KEY / ILAR_OPENAI_API_KEY)"))?,
    );

        let session_id = match &resume_target {
            Some(id) => {
                if cli_model.is_some()
                    && persisted_model.as_deref() != Some(model_for_session.as_str())
                {
                    persist_model_change(resolver.as_ref(), &store, id, &model_for_session, None)
                        .with_context(|| format!("persisting model override {model_for_session}"))?;
                }
                id.clone()
            }
            None => {
                let id = new_id();
                store
                    .create(SessionMeta {
                        session_id: id.clone(),
                        parent_id: None,
                        agent: agent.name.clone(),
                        model: model_for_session.clone(),
                        workspace: None,
                    })
                    .context("creating session")?;
                id
            }
        };

        let loop_config = LoopConfig {
            compaction_threshold: config.compaction.threshold,
            max_iterations: config.agent.max_iterations,
            ..LoopConfig::default()
        };
        let services = ilar::tools::service::ServiceManager::new();
        let spawner = std::sync::Arc::new(
            SubagentSpawner::new(
                resolver.clone(),
                store.clone(),
                agents,
                cwd.clone(),
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
        let todos = std::sync::Arc::new(std::sync::Mutex::new(restored_todos(resumed.as_ref())));
        let registry = ToolRegistry::builtin()
            .with_subagents(spawner.clone())?
            .with_services(services.clone())?
            .with_models(config.available_models())?
            .with_todos(todos.clone())?
            .with_web_tools()?
            .with_skills(skill_store)?;
        let notifications = spawner.subscribe();
        let subagent_activity = spawner.subscribe_activity();
        let tool_ctx = ToolContext::root(cwd.clone()).with_subagents(spawner.clone());
        let model_choices = config.available_models();
        let user_config_path = config.dirs().0.join("ilar.toml");

        let (context_used, context_estimated) =
            session_context_tokens(&store, &session_id, &system_prompt, &registry)?;
        let context_limit = display_context_limit(resolver.as_ref(), &model_for_session);
        let mut app = App::new();
        app.theme = active_theme;
        app.history = history::PromptHistory::load(config.state_dir().join("prompt_history.jsonl"));
        app.skills = skill_inventory;
        app.commands = command_inventory;
        app.session_id = session_id.clone();
        app.todos = todos;
        if let Some(resumed) = &resumed {
            app.restore_session(resumed, &store);
        }
        app.configure_runtime(
            model_for_session.clone(),
            (persisted_model.as_deref() == Some(model_for_session.as_str()))
                .then_some(persisted_variant)
                .flatten(),
            cwd.clone(),
            context_used,
            context_limit,
            context_estimated,
        );

        if terminal_hold.is_none() {
            terminal_hold = Some(TerminalSession::start()?);
        }
        app.keyboard_enhanced = terminal_hold
            .as_ref()
            .is_some_and(|(_, session)| session.keyboard_enhanced);
        let terminal = &mut terminal_hold.as_mut().expect("terminal started").0;
        let exit = run_app(
            terminal,
            &mut app,
            &user_config_path,
            resolver,
            &store,
            &session_id,
            &system_prompt,
            &registry,
            tool_ctx,
            spawner,
            notifications,
            subagent_activity,
            loop_config,
            model_choices,
            services,
        )
        .await?;
        active_theme = app.theme;
        match exit {
            AppExit::Quit => return Ok(()),
            AppExit::Switch(next) => session_override = Some(next),
        }
    } // session loop
}

/// The meter must show the limit compaction actually measures against —
/// the provider's input cap, not the whole window. Showing the window
/// reads as comfortable headroom while the request is already too big.
fn display_context_limit(resolver: &dyn ProviderResolver, model: &str) -> Option<u64> {
    resolver
        .compaction_limit(model)
        .or_else(|| resolver.context_limit(model))
}

fn session_context_tokens(
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
) -> Result<(u64, bool)> {
    let session = store.load(session_id)?;
    let estimated = ilar::compaction::estimate_reader_tokens_with_request(
        &session,
        Some(system_prompt),
        &registry.definitions(),
    );
    Ok((estimated, true))
}

struct PendingNotification {
    notification: ilar::subagent::Notification,
    queued_ahead: usize,
}

fn next_notification(
    blocked: bool,
    pending: &mut Option<PendingNotification>,
    notifications: &mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
) -> Option<ilar::subagent::Notification> {
    if blocked {
        return None;
    }
    if pending
        .as_ref()
        .is_some_and(|pending| pending.queued_ahead == 0)
    {
        return pending.take().map(|pending| pending.notification);
    }
    match notifications.try_recv() {
        Ok(notification) => {
            if let Some(pending) = pending {
                pending.queued_ahead = pending.queued_ahead.saturating_sub(1);
            }
            Some(notification)
        }
        Err(_) => pending.take().map(|pending| pending.notification),
    }
}

enum TurnCompletion {
    Root(Result<TurnOutcome>),
    Routed(Result<ilar::subagent::RouteOutcome>),
}

fn ring_terminal_bell_if_idle(
    writer: &mut impl std::io::Write,
    pending: &mut bool,
    turn_active: bool,
) -> std::io::Result<bool> {
    if !*pending || turn_active {
        return Ok(false);
    }
    *pending = false;
    writer.write_all(b"\x07")?;
    writer.flush()?;
    Ok(true)
}

struct WheelBatch {
    rows: isize,
    deferred: Option<Event>,
}

fn wheel_rows(event: &Event) -> Option<isize> {
    match event {
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollUp => Some(-3),
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::ScrollDown => Some(3),
        _ => None,
    }
}

fn drain_wheel_batch(
    initial_rows: isize,
    max_events: usize,
    mut try_next: impl FnMut() -> Result<Option<Event>>,
) -> Result<WheelBatch> {
    let mut rows = initial_rows;
    let mut events = 1usize;
    while events < max_events.max(1) {
        let Some(event) = try_next()? else {
            return Ok(WheelBatch {
                rows,
                deferred: None,
            });
        };
        if let Some(next_rows) = wheel_rows(&event) {
            rows = rows.saturating_add(next_rows);
            events += 1;
        } else {
            return Ok(WheelBatch {
                rows,
                deferred: Some(event),
            });
        }
    }
    Ok(WheelBatch {
        rows,
        deferred: None,
    })
}

/// How run_app ended: quit the program, or restart against another session.
enum AppExit {
    Quit,
    Switch(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    user_config_path: &std::path::Path,
    resolver: Arc<dyn ProviderResolver>,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    tool_ctx: ToolContext,
    spawner: std::sync::Arc<ilar::subagent::SubagentSpawner>,
    mut notifications: tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    mut subagent_activity: tokio::sync::broadcast::Receiver<ilar::subagent::SubagentActivity>,
    loop_config: LoopConfig,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
    services: std::sync::Arc<ilar::tools::service::ServiceManager>,
) -> Result<AppExit> {
    let mut events_rx: Option<LoopEventReceiver> = None;
    let mut pending_notification = None;
    let mut notifications_paused = false;
    let mut cancel: Option<CancellationToken> = None;
    // Live only while a root turn runs, so a message typed during that
    // turn is steered into it. Cross-session routed turns have no
    // channel and still queue.
    let mut steer_tx: Option<ilar::agent::SteerSender> = None;
    let mut turn_handle: Option<tokio::task::JoinHandle<TurnCompletion>> = None;
    let mut ring_on_turn_completion = false;
    let mut bell_pending = false;
    let mut pending_terminal_event = None;
    // Decisions accumulate here and are performed in one place below,
    // rather than each arm doing its own effects inline.
    let mut intents: Vec<Intent> = Vec::new();

    loop {
        // Drain pending loop events.
        if let Some(rx) = events_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                app.push_loop_event(&event);
            }
        }
        app.retry_subagent_activity();
        for _ in 0..256 {
            let Ok(activity) = subagent_activity.try_recv() else {
                break;
            };
            app.push_subagent_activity(&activity);
        }
        // Turn finished? Join and clean up.
        if let Some(handle) = turn_handle.as_mut()
            && handle.is_finished()
        {
            let handle = turn_handle.take().unwrap();
            bell_pending |= std::mem::take(&mut ring_on_turn_completion);
            if let Some(rx) = events_rx.as_mut() {
                while let Ok(event) = rx.try_recv() {
                    app.push_loop_event(&event);
                }
            }
            match handle.await {
                Ok(TurnCompletion::Root(result)) => {
                    let aborted = matches!(result, Ok(TurnOutcome::Aborted));
                    let completed = matches!(result, Ok(TurnOutcome::Completed));
                    app.finish_turn(result);
                    if !aborted {
                        notifications_paused = false;
                    }
                    if aborted && let Some((_, round)) = &app.goal {
                        let message = format!(
                            "goal paused (round {round}/{MAX_GOAL_ROUNDS}) — resumes after your next completed turn; Ctrl-Q to manage"
                        );
                        app.push_transcript_line(Line_::System(message.clone()));
                        app.set_notice(message, NoticeLevel::Warning);
                    }
                    let state = observe(
                        app,
                        &turn_handle,
                        &pending_terminal_event,
                        &steer_tx,
                        notifications_paused,
                    );
                    let round = app.goal.as_ref().map(|(_, round)| *round);
                    // Only scan the transcript when there is a goal to
                    // satisfy; every other turn pays nothing.
                    let achieved = round.is_some()
                        && app
                            .lines
                            .iter()
                            .rev()
                            .find_map(|line| match line {
                                Line_::Assistant(text) => Some(goal_achieved_in(text)),
                                _ => None,
                            })
                            .unwrap_or(false);
                    let goal = app
                        .goal
                        .as_ref()
                        .map(|(goal, round)| (goal.clone(), *round));
                    intents.extend(after_turn(
                        &state,
                        completed,
                        goal.as_ref().map(|(goal, round)| (goal.as_str(), *round)),
                        achieved,
                        MAX_GOAL_ROUNDS,
                    ));
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Propagate(
                    notification,
                )))) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.clear_transient_notice();
                    app.set_activity(Activity::Ready);
                    pending_notification = Some(PendingNotification {
                        notification,
                        queued_ahead: notifications.len(),
                    });
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Requeue(
                    notification,
                )))) => {
                    app.busy = false;
                    app.status = "notification paused; send a message to resume".into();
                    app.set_persistent_notice(
                        "notification paused; send a message to resume",
                        NoticeLevel::Warning,
                    );
                    app.set_activity(Activity::Paused);
                    pending_notification = Some(PendingNotification {
                        notification,
                        queued_ahead: 0,
                    });
                    notifications_paused = true;
                }
                Ok(TurnCompletion::Routed(Ok(ilar::subagent::RouteOutcome::Complete))) => {
                    app.busy = false;
                    app.status = "ready".into();
                    app.clear_transient_notice();
                    app.set_activity(Activity::Ready);
                }
                Ok(TurnCompletion::Routed(Err(error))) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    let message = format!("notification routing failed: {error}");
                    app.set_notice(&message, NoticeLevel::Error);
                    app.push_transcript_line(Line_::System(message));
                }
                Err(error) => {
                    app.busy = false;
                    app.status = "error".into();
                    app.set_activity(Activity::Error);
                    let message = format!("notification routing failed: {error}");
                    app.set_notice(&message, NoticeLevel::Error);
                    app.push_transcript_line(Line_::System(message));
                }
            }
            events_rx = None;
            cancel = None;
            steer_tx = None;
            // The turn dropped its receiver. Anything it never delivered
            // (an abort, an error) would otherwise vanish with no
            // transcript line and no way to get it back.
            if !app.pending_steers.is_empty() {
                let undelivered = std::mem::take(&mut app.pending_steers);
                let count = undelivered.len();
                app.queued_messages.splice(0..0, undelivered);
                app.set_notice(
                    format!("{count} undelivered steer(s) moved to the queue — Ctrl-Q to review"),
                    NoticeLevel::Warning,
                );
            }
        }

        // Perform what was decided. `apply_intent` owns the state
        // changes and is testable on an App alone; spawning is the only
        // thing that needs the runtime, so it stays here.
        for intent in std::mem::take(&mut intents) {
            let Some(text) = apply_intent(app, intent, steer_tx.as_ref()) else {
                continue;
            };
            // Every push site is guarded against starting a turn while
            // one runs. If that ever slips, the running turn would be
            // orphaned — its cancellation token dropped without firing,
            // still writing to the same session.
            debug_assert!(
                turn_handle.is_none(),
                "starting a turn while one is already running"
            );
            let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
            events_rx = Some(rx);
            let token = CancellationToken::new();
            cancel = Some(token.clone());
            let resolver = resolver.clone();
            let store = store.clone();
            let session_id = session_id.to_string();
            let system_prompt = system_prompt.to_string();
            let registry = registry.clone();
            let turn_ctx = tool_ctx.clone();
            let mut loop_config = loop_config.clone();
            if std::mem::take(&mut app.compact_requested) {
                loop_config.force_compaction = true;
            }
            ring_on_turn_completion = true;
            let (tx_steer, steer_rx) = ilar::agent::steer_channel();
            steer_tx = Some(tx_steer);
            turn_handle = Some(tokio::spawn(async move {
                TurnCompletion::Root(
                    run_turn(
                        resolver.as_ref(),
                        &registry,
                        &store,
                        &session_id,
                        &text,
                        Some(&system_prompt),
                        loop_config,
                        tx,
                        token,
                        turn_ctx,
                        Some(steer_rx),
                    )
                    .await,
                )
            }));
        }

        // Let a buffered Ctrl-P open the palette before starting queued work.
        let modal_open = app.has_modal();
        if turn_handle.is_none()
            && !notifications_paused
            && !modal_open
            && pending_terminal_event.is_none()
            && crossterm::event::poll(std::time::Duration::ZERO)?
        {
            pending_terminal_event = Some(crossterm::event::read()?);
        }
        if pending_terminal_event
            .as_ref()
            .is_some_and(is_command_palette_shortcut)
        {
            pending_terminal_event = None;
            app.model_key_pending = false;
            if app.search_active {
                app.close_search(false);
            }
            app.open_command_palette();
        }
        // Background completions re-invoke their declared parent while idle.
        let state = observe(
            app,
            &turn_handle,
            &pending_terminal_event,
            &steer_tx,
            notifications_paused,
        );
        if let Some(notification) = next_notification(
            !may_route_notification(&state),
            &mut pending_notification,
            &mut notifications,
        ) {
            if notification.parent_session_id != session_id {
                let token = CancellationToken::new();
                cancel = Some(token.clone());
                app.busy = true;
                app.status = format!("routing task to {}", notification.parent_session_id);
                app.clear_transient_notice();
                app.set_activity(Activity::Thinking);
                let spawner = spawner.clone();
                turn_handle = Some(tokio::spawn(async move {
                    TurnCompletion::Routed(spawner.route_notification(notification, token).await)
                }));
                continue;
            }
            let text = notification.text;
            app.push_notification(&notification.description, &text);
            let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
            events_rx = Some(rx);
            let token = CancellationToken::new();
            cancel = Some(token.clone());
            app.busy = true;
            app.status = "thinking".into();
            app.clear_transient_notice();
            app.set_activity(Activity::Thinking);
            let resolver = resolver.clone();
            let store = store.clone();
            let session_id = session_id.to_string();
            let system_prompt = system_prompt.to_string();
            let registry = registry.clone();
            let turn_ctx = tool_ctx.clone();
            let loop_config = loop_config.clone();
            let (tx_steer, steer_rx) = ilar::agent::steer_channel();
            steer_tx = Some(tx_steer);
            turn_handle = Some(tokio::spawn(async move {
                TurnCompletion::Root(
                    run_turn(
                        resolver.as_ref(),
                        &registry,
                        &store,
                        &session_id,
                        &text,
                        Some(&system_prompt),
                        loop_config,
                        tx,
                        token,
                        turn_ctx,
                        Some(steer_rx),
                    )
                    .await,
                )
            }));
        }

        let _ = ring_terminal_bell_if_idle(
            &mut std::io::stdout(),
            &mut bell_pending,
            turn_handle.is_some(),
        );

        app.background_running = spawner.running_background();
        app.services_view = services.snapshot();
        app.services_running = app
            .services_view
            .iter()
            .filter(|(_, running, _)| *running)
            .count();
        terminal.draw(|frame| app.render(frame))?;

        // Poll terminal input (fast while busy so streaming keeps rendering).
        let timeout = if app.busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        let event = if let Some(event) = pending_terminal_event.take() {
            event
        } else {
            if !crossterm::event::poll(timeout)? {
                continue;
            }
            crossterm::event::read()?
        };
        match event {
            Event::Key(
                key @ KeyEvent {
                    code,
                    kind: KeyEventKind::Press | KeyEventKind::Repeat,
                    modifiers,
                    ..
                },
            ) => {
                let control = modifiers.contains(KeyModifiers::CONTROL);
                if matches!((code, control), (KeyCode::Char('c'), true)) {
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                    spawner.shutdown().await;
                    return Ok(AppExit::Quit);
                }
                if app.active_modal() == Some(Modal::PendingManager) {
                    match app.pending_manager_key(code, control) {
                        PendingAction::Stay => {}
                        PendingAction::Close => app.pending_manager = None,
                        PendingAction::DeleteQueued(index) => {
                            if index < app.queued_messages.len() {
                                let removed = app.queued_messages.remove(index);
                                app.set_notice(
                                    format!(
                                        "removed queued message: {}",
                                        removed.lines().next().unwrap_or("")
                                    ),
                                    NoticeLevel::Info,
                                );
                            }
                        }
                        PendingAction::EditQueued(index) => {
                            if index < app.queued_messages.len() {
                                let message = app.queued_messages.remove(index);
                                app.input = InputBuffer::from(message);
                                app.pending_manager = None;
                            }
                        }
                        PendingAction::AbortGoal => {
                            if let Some((goal, round)) = app.goal.take() {
                                let message =
                                    format!("goal aborted after {round} round(s): {goal}");
                                app.push_transcript_line(Line_::System(message.clone()));
                                app.set_notice(message, NoticeLevel::Info);
                            }
                        }
                        PendingAction::EditGoal => {
                            if let Some((goal, _)) = &app.goal {
                                app.input = InputBuffer::from(format!("/goal {goal}"));
                                app.pending_manager = None;
                            }
                        }
                        PendingAction::CancelBackground => {
                            spawner.abort_all();
                            notifications_paused = true;
                            app.background_running = 0;
                            app.set_persistent_notice(
                                "background jobs cancelled; notifications paused; send a message to resume",
                                NoticeLevel::Warning,
                            );
                        }
                        PendingAction::StopServices => {
                            services.stop_all();
                            app.services_running = 0;
                            app.set_notice("services stopped", NoticeLevel::Info);
                        }
                        PendingAction::DismissRetry => {
                            app.retry_available = false;
                            app.clear_transient_notice();
                        }
                        PendingAction::RetryNow => {
                            if !app.busy && turn_handle.is_none() {
                                let state = observe(
                                    app,
                                    &turn_handle,
                                    &pending_terminal_event,
                                    &steer_tx,
                                    notifications_paused,
                                );
                                let decided = retry_intents(&state, app.last_prompt.as_deref());
                                if decided.iter().any(|i| matches!(i, Intent::StartTurn(_))) {
                                    app.pending_manager = None;
                                }
                                intents.extend(decided);
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::Help) {
                    match code {
                        KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
                        KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
                        KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(10),
                        KeyCode::PageDown => app.help_scroll = app.help_scroll.saturating_add(10),
                        KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?' | 'q') => {
                            app.help_visible = false;
                            app.help_scroll = 0;
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::ThemePicker) {
                    let action = {
                        let picker = app.theme_picker.as_mut().unwrap();
                        picker.handle_key(code, control)
                    };
                    apply_theme_picker_action(app, action, |selected| {
                        ilar::config::persist_general_theme(user_config_path, selected.id())
                    });
                    continue;
                }
                if app.active_modal() == Some(Modal::SkillPicker)
                    && let Some(picker) = app.skill_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.skill_picker = None;
                        }
                        PickerAction::Choose(name) => {
                            app.skill_picker = None;
                            app.input = InputBuffer::from(format!("/{name} "));
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::SessionPicker)
                    && let Some(picker) = app.session_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        SessionPickerAction::Stay => {}
                        SessionPickerAction::Dismiss => {
                            app.session_picker = None;
                            app.clear_transient_notice();
                        }
                        SessionPickerAction::Delete(id) => match store.delete(&id) {
                            Ok(()) => {
                                if let Some(picker) = app.session_picker.as_mut() {
                                    picker.sessions.retain(|session| session.id != id);
                                    picker.selected = 0;
                                }
                                app.set_notice(format!("deleted session {id}"), NoticeLevel::Info);
                            }
                            Err(error) => {
                                app.set_notice(
                                    format!("cannot delete {id}: {error}"),
                                    NoticeLevel::Error,
                                );
                            }
                        },
                        SessionPickerAction::Fork(id) => {
                            let blocked = if turn_handle.is_some() {
                                Some("finish or abort the current turn before switching sessions")
                            } else if spawner.running_background() > 0 {
                                Some("background agents are running; wait or abort them first")
                            } else if !app.input.is_blank() {
                                Some("input has an unsent draft; send or clear it first")
                            } else {
                                None
                            };
                            if let Some(reason) = blocked {
                                app.set_notice(reason, NoticeLevel::Warning);
                                continue;
                            }
                            match store.fork(&id) {
                                Ok(fork_id) => {
                                    spawner.shutdown().await;
                                    return Ok(AppExit::Switch(fork_id));
                                }
                                Err(error) => {
                                    app.set_notice(
                                        format!("cannot fork {id}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                            }
                        }
                        SessionPickerAction::Resume(new_session) => {
                            let blocked = if turn_handle.is_some() {
                                Some("finish or abort the current turn before switching sessions")
                            } else if spawner.running_background() > 0 {
                                Some("background agents are running; wait or abort them first")
                            } else if !app.input.is_blank() {
                                Some("input has an unsent draft; send or clear it first")
                            } else {
                                None
                            };
                            if let Some(reason) = blocked {
                                app.set_notice(reason, NoticeLevel::Warning);
                                continue;
                            }
                            // Validate now so a bad entry degrades to a
                            // notice instead of exiting the app later.
                            match store
                                .load(&new_session)
                                .map(|session| ensure_direct_resume_allowed(session.meta()))
                            {
                                Ok(Ok(())) => {
                                    spawner.shutdown().await;
                                    return Ok(AppExit::Switch(new_session));
                                }
                                Ok(Err(error)) => {
                                    app.set_notice(
                                        format!("cannot resume {new_session}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                                Err(error) => {
                                    app.set_notice(
                                        format!("cannot resume {new_session}: {error}"),
                                        NoticeLevel::Error,
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::ModelPicker)
                    && let Some(picker) = app.model_picker.as_mut()
                {
                    match picker.handle_key(code, control) {
                        PickerAction::Stay => {}
                        PickerAction::Dismiss => {
                            app.model_picker = None;
                            app.status = "ready".into();
                            app.clear_transient_notice();
                        }
                        PickerAction::Choose(new_model) => {
                            if let Some(model) = ilar::model::find(&new_model)
                                && !model.variants().is_empty()
                            {
                                app.clear_transient_notice();
                                let active_variant = (new_model == app.current_model)
                                    .then_some(app.current_variant.as_deref())
                                    .flatten();
                                app.model_picker = None;
                                app.variant_picker =
                                    Some(VariantPicker::new(model, active_variant));
                                continue;
                            }
                            match adopt_model_selection(
                                app,
                                resolver.as_ref(),
                                store,
                                session_id,
                                system_prompt,
                                registry,
                                new_model.clone(),
                                None,
                            ) {
                                Ok(()) => {
                                    app.model_picker = None;
                                }
                                Err(error) => {
                                    if let Some(picker) = app.model_picker.as_mut() {
                                        picker.error =
                                            Some(format!("cannot switch to {new_model}: {error}"));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::VariantPicker) {
                    let (action, model) = {
                        let picker = app.variant_picker.as_mut().unwrap();
                        (picker.handle_key(code, control), picker.model.full_id())
                    };
                    match action {
                        VariantPickerAction::Stay => {}
                        VariantPickerAction::Dismiss => {
                            app.variant_picker = None;
                            app.clear_transient_notice();
                        }
                        VariantPickerAction::Choose(variant) => {
                            match adopt_model_selection(
                                app,
                                resolver.as_ref(),
                                store,
                                session_id,
                                system_prompt,
                                registry,
                                model.clone(),
                                variant,
                            ) {
                                Ok(()) => app.variant_picker = None,
                                Err(error) => {
                                    if let Some(picker) = app.variant_picker.as_mut() {
                                        picker.error = Some(format!(
                                            "cannot switch reasoning for {model}: {error}"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::Search) {
                    match (code, control) {
                        (KeyCode::Esc, _) => app.close_search(true),
                        (KeyCode::Enter, _) => app.close_search(false),
                        (KeyCode::Up, _) | (KeyCode::Char('p'), true) => app.search_jump(-1),
                        (KeyCode::Down, _) | (KeyCode::Char('n'), true) => app.search_jump(1),
                        (KeyCode::Char('f'), true) => app.close_search(false),
                        (KeyCode::Backspace, _) => {
                            app.search_query.pop();
                            app.search_refresh();
                        }
                        (KeyCode::Char(character), false) if !character.is_control() => {
                            app.search_query.push(character);
                            app.search_refresh();
                        }
                        _ => {}
                    }
                    continue;
                }
                if app.active_modal() == Some(Modal::CommandPalette)
                    && let Some(palette) = app.command_palette.as_mut()
                {
                    match palette.handle_key(code, control) {
                        CommandPaletteAction::Stay => {}
                        CommandPaletteAction::Dismiss => {
                            app.command_palette = None;
                        }
                        CommandPaletteAction::Choose(command) => {
                            activate_palette_command(app, command, model_choices.clone());
                            if std::mem::take(&mut app.session_picker_requested) {
                                let sessions = store
                                    .list()
                                    .into_iter()
                                    .filter(|session| session.id != app.session_id)
                                    .collect();
                                app.session_picker = Some(SessionPicker::new(sessions));
                            }
                        }
                    }
                    continue;
                }
                if matches!((code, control), (KeyCode::Char('p'), true)) {
                    app.model_key_pending = false;
                    app.open_command_palette();
                    continue;
                }
                if app.model_key_pending {
                    app.model_key_pending = false;
                    if code == KeyCode::Esc {
                        app.status = "ready".into();
                        app.clear_transient_notice();
                        continue;
                    }
                    if matches!(code, KeyCode::Char('m' | 'M'))
                        && !app.busy
                        && !model_choices.is_empty()
                    {
                        app.clear_transient_notice();
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                        continue;
                    }
                    if matches!(code, KeyCode::Char('t' | 'T')) && !app.busy {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                        continue;
                    }
                    app.status = "ready".into();
                    app.clear_transient_notice();
                }
                if matches!((code, control), (KeyCode::Char('x'), true)) && !app.busy {
                    app.model_key_pending = true;
                    app.status = "Ctrl-X: M models · T themes".into();
                    app.set_notice("Ctrl-X: M models · T themes", NoticeLevel::Info);
                    continue;
                }
                match (code, control) {
                    (KeyCode::F(1), _) => {
                        app.help_visible = true;
                        app.help_scroll = 0;
                    }
                    // Ctrl-M is simply unreachable without keyboard
                    // enhancement (the terminal reports it as Enter);
                    // the arm stays for terminals that do report it.
                    (KeyCode::Char('m'), true) | (KeyCode::F(2), false)
                        if !app.busy && !model_choices.is_empty() =>
                    {
                        app.clear_transient_notice();
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                    }
                    (KeyCode::F(3), false) if !app.busy => {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                    }
                    (KeyCode::Esc, _) => {
                        // Esc is strictly immediate-scope: abort the running
                        // turn or clear the input. Standing state (goal,
                        // queue, background jobs) lives in the pending
                        // manager (Ctrl-Q) and explicit commands.
                        if app.busy {
                            if let Some(cancel) = &cancel {
                                cancel.cancel();
                                app.status = "aborting…".into();
                                app.set_notice("aborting turn…", NoticeLevel::Warning);
                                app.set_activity(Activity::Aborting);
                            }
                        } else if !app.input.is_blank() {
                            app.input.clear();
                        }
                    }
                    // Ctrl-U edits the input when it has text; the
                    // half-page scroll needs a blank input.
                    (KeyCode::Char('u'), true) if app.input.is_blank() => {
                        app.scroll_up(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Char('d'), true) if app.input.is_blank() => {
                        app.scroll_down(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Home, true) => app.scroll_to_top(),
                    (KeyCode::End, true) => app.scroll_to_tail(),
                    (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                    (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                    // History recall wins while browsing or on a blank
                    // input; transcript scrolling stays on PgUp/wheel/^U.
                    (KeyCode::Up, _)
                        if app.history.browsing()
                            || (!app.input.is_multiline() && app.input.is_blank()) =>
                    {
                        if let Some(text) = app.history.previous(app.input.text()) {
                            app.input = InputBuffer::from(text);
                        } else if !app.history.browsing() {
                            app.scroll_up(1);
                        }
                    }
                    (KeyCode::Down, _) if app.history.browsing() => {
                        if let Some(text) = app.history.next(app.input.text()) {
                            app.input = InputBuffer::from(text);
                        }
                    }
                    (KeyCode::Up, _) if !app.input.is_multiline() => app.scroll_up(1),
                    (KeyCode::Down, _) if !app.input.is_multiline() => app.scroll_down(1),
                    (KeyCode::Char('f'), true) => {
                        app.open_search();
                    }
                    (KeyCode::Char('q'), true) => {
                        app.pending_manager = Some(PendingManager::default());
                    }
                    (code, control)
                        if retry_requested(code, control)
                            && app.retry_available
                            && !app.busy
                            && turn_handle.is_none() =>
                    {
                        let state = observe(
                            app,
                            &turn_handle,
                            &pending_terminal_event,
                            &steer_tx,
                            notifications_paused,
                        );
                        intents.extend(retry_intents(&state, app.last_prompt.as_deref()));
                    }
                    // Inline slash completion: navigate/accept while the
                    // command name is being typed.
                    (KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter, false)
                        if !slash_candidates(app.input.text(), &app.slash_inventory())
                            .is_empty()
                            && !key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        let candidates = slash_candidates(app.input.text(), &app.slash_inventory());
                        app.slash_selected = app.slash_selected.min(candidates.len() - 1);
                        match code {
                            KeyCode::Up => {
                                app.slash_selected =
                                    (app.slash_selected + candidates.len() - 1) % candidates.len();
                            }
                            KeyCode::Down => {
                                app.slash_selected = (app.slash_selected + 1) % candidates.len();
                            }
                            // Tab and Enter both accept the selection; the
                            // completed input ("/name ") hides the popup, so
                            // a second Enter submits as usual.
                            KeyCode::Tab | KeyCode::Enter => {
                                let (name, _) = &candidates[app.slash_selected];
                                app.input = InputBuffer::from(format!("/{name} "));
                                app.slash_selected = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => match handle_prompt_key(&mut app.input, key) {
                        PromptAction::Submit if !app.input.is_blank() => {
                            // Observe before taking the text: the
                            // decision is about the state the user
                            // submitted into.
                            let state = observe(
                                app,
                                &turn_handle,
                                &pending_terminal_event,
                                &steer_tx,
                                notifications_paused,
                            );
                            let text = app.input.take();
                            app.history.push(&text);
                            // The transcript line for a steer appears
                            // when the loop delivers it, not on submit.
                            let decided = decide::submit(&state, app.busy, text);
                            apply_event_intents(app, decided, &mut intents, steer_tx.as_ref());
                        }
                        PromptAction::Edited => app.clear_transient_notice(),
                        PromptAction::Unhandled | PromptAction::Submit => {}
                    },
                }
            }
            Event::Paste(text) => {
                let state = observe(
                    app,
                    &turn_handle,
                    &pending_terminal_event,
                    &steer_tx,
                    notifications_paused,
                );
                let decided = decide::paste(&state, text);
                apply_event_intents(app, decided, &mut intents, steer_tx.as_ref());
            }
            Event::Mouse(mouse)
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                let initial_rows = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                let batch = drain_wheel_batch(initial_rows, MAX_WHEEL_EVENTS_PER_BATCH, || {
                    if crossterm::event::poll(std::time::Duration::ZERO)? {
                        Ok(Some(crossterm::event::read()?))
                    } else {
                        Ok(None)
                    }
                })?;
                pending_terminal_event = batch.deferred;
                // The overlay in front gets first refusal; a 45-entry
                // model picker should scroll like everything else.
                if !app.scroll_active_modal(batch.rows) {
                    app.scroll_wheel(batch.rows);
                }
            }
            Event::Mouse(mouse)
                if app
                    .active_modal()
                    .is_none_or(|modal| modal == Modal::Search) =>
            {
                // Search is a transcript-reading mode, so selecting and
                // expanding must keep working underneath it.
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        app.begin_transcript_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        app.drag_transcript_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(text) = app.finish_transcript_selection(mouse.column, mouse.row)
                            && let Err(error) = app.copy_to_clipboard(&text)
                        {
                            let message = format!("clipboard copy failed: {error:#}");
                            app.set_notice(&message, NoticeLevel::Error);
                            app.push_transcript_line(Line_::System(message));
                            app.follow_tail = true;
                            app.set_activity(Activity::Error);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The meter must not show the whole window while compaction is
    /// measuring against the input cap — that reads as comfortable
    /// headroom when the request is already too big to send.
    #[test]
    fn context_meter_uses_the_same_limit_as_compaction() {
        struct SplitLimits;
        impl ProviderResolver for SplitLimits {
            fn resolve_provider(&self, _: &str) -> Result<ilar::provider::ProviderHandle<'_>> {
                anyhow::bail!("unused")
            }
            fn context_limit(&self, _: &str) -> Option<u64> {
                Some(128_000)
            }
            fn input_limit(&self, _: &str) -> Option<u64> {
                Some(100_000)
            }
        }
        assert_eq!(
            display_context_limit(&SplitLimits, "openai/gpt-5.3-codex-spark"),
            Some(100_000),
            "meter showed the full window instead of the input cap"
        );

        struct WindowOnly;
        impl ProviderResolver for WindowOnly {
            fn resolve_provider(&self, _: &str) -> Result<ilar::provider::ProviderHandle<'_>> {
                anyhow::bail!("unused")
            }
            fn context_limit(&self, _: &str) -> Option<u64> {
                Some(64_000)
            }
        }
        assert_eq!(
            display_context_limit(&WindowOnly, "custom/model"),
            Some(64_000)
        );
    }

    #[test]
    fn startup_selection_respects_cli_and_persisted_precedence() {
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
    }

    #[test]
    fn goal_sentinel_detection_requires_line_start() {
        assert!(goal_achieved_in(
            "done!\nGOAL_ACHIEVED: 5/5 turns replay at 92%"
        ));
        assert!(goal_achieved_in("  GOAL_ACHIEVED: evidence attached"));
        assert!(!goal_achieved_in(
            "we still need to reach GOAL_ACHIEVED status later"
        ));
        assert!(!goal_achieved_in("no sentinel here"));

        let kickoff = goal_kickoff_prompt("replay 5 turns at 90%");
        assert!(kickoff.contains("replay 5 turns at 90%"));
        assert!(kickoff.contains("evidence"), "{kickoff}");
        let cont = goal_continuation_prompt("replay 5 turns at 90%", 3);
        assert!(cont.contains("round 3/25"), "{cont}");
        assert!(cont.contains("GOAL_ACHIEVED"), "{cont}");
    }

    #[test]
    fn startup_context_estimate_includes_prompt_and_tools() {
        let root = std::env::temp_dir().join(format!("ilar-tui-context-{}", new_id()));
        let store = SessionStore::new(root.clone());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );
        let system_prompt = "system context ".repeat(100);

        let (tokens, estimated) = session_context_tokens(
            &store,
            &session_id,
            &system_prompt,
            &ToolRegistry::read_only(),
        )
        .unwrap();

        assert!(estimated);
        assert!(tokens >= system_prompt.chars().count() as u64 / 4);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn direct_resume_rejects_workspace_bound_child_sessions() {
        let meta = SessionMeta {
            session_id: new_id(),
            parent_id: Some(new_id()),
            agent: "explore".into(),
            model: "zai/glm-4.7".into(),
            workspace: Some(ilar::tools::WorkspaceLocation::shared(std::env::temp_dir())),
        };

        let error = ensure_direct_resume_allowed(Some(&meta)).unwrap_err();

        assert!(error.to_string().contains("through Task"), "{error:#}");
        assert!(ensure_direct_resume_allowed(None).is_ok());
    }

    #[test]
    fn model_change_is_adopted_only_after_persistence() {
        let root = std::env::temp_dir().join(format!("ilar-tui-model-{}", new_id()));
        let store = SessionStore::new(root.clone());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                })
                .unwrap(),
        );
        let resolver = ilar::provider::MockProvider::default();

        let writer = store.acquire_writer(&session_id).unwrap();
        assert!(
            persist_model_change(&resolver, &store, &session_id, "openai/gpt-5.2", None).is_err()
        );
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "zai/glm-4.7"
        );
        drop(writer);

        persist_model_change(
            &resolver,
            &store,
            &session_id,
            "openai/gpt-5.2",
            Some("high"),
        )
        .unwrap();
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "openai/gpt-5.2"
        );
        assert_eq!(
            store.load(&session_id).unwrap().effective_variant(),
            Some("high".into())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn slash_invocations_parse_and_rewrite() {
        assert_eq!(
            parse_slash_invocation("/deploy  to staging "),
            Some(("deploy", "to staging"))
        );
        assert_eq!(parse_slash_invocation("/deploy"), Some(("deploy", "")));
        assert_eq!(parse_slash_invocation("plain prompt"), None);
        assert_eq!(parse_slash_invocation("/"), None);
        assert_eq!(parse_slash_invocation("/etc/passwd is odd"), None);
        assert_eq!(parse_slash_invocation("/ leading space"), None);

        let prompt = skill_invocation_prompt("deploy", "to staging");
        assert!(prompt.contains("`skill` tool"), "{prompt}");
        assert!(prompt.contains("\"deploy\""), "{prompt}");
        assert!(prompt.contains("to staging"), "{prompt}");
        assert!(
            !skill_invocation_prompt("deploy", "").contains("Arguments"),
            "argless invocations skip the arguments clause"
        );

        let skills = vec![
            ("deploy".to_string(), "d".to_string()),
            ("release-notes".to_string(), "r".to_string()),
        ];
        assert_eq!(close_skill_matches(&skills, "rel"), vec!["release-notes"]);
        assert_eq!(
            close_skill_matches(&skills, "zzz"),
            vec!["deploy", "release-notes"],
            "no match falls back to the full (bounded) list"
        );
    }

    #[test]
    fn terminal_bell_waits_for_idle_and_only_writes_once() {
        let mut output = Vec::new();
        let mut pending = true;

        assert!(!ring_terminal_bell_if_idle(&mut output, &mut pending, true).unwrap());
        assert!(output.is_empty());
        assert!(pending);

        assert!(ring_terminal_bell_if_idle(&mut output, &mut pending, false).unwrap());
        assert_eq!(output, b"\x07");
        assert!(!pending);

        assert!(!ring_terminal_bell_if_idle(&mut output, &mut pending, false).unwrap());
        assert_eq!(output, b"\x07");
    }

    #[test]
    fn queued_wheel_events_are_coalesced_until_the_next_distinct_input() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let mut queued = vec![
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            key,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
        ]
        .into_iter();

        let batch = drain_wheel_batch(-3, 32, || Ok(queued.next())).unwrap();

        assert_eq!(batch.rows, -3);
        assert!(matches!(
            batch.deferred,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Char('x'),
                ..
            }))
        ));
        assert!(matches!(
            queued.next(),
            Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                ..
            }))
        ));
    }

    #[test]
    fn resumed_todos_seed_the_first_shared_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::ToolCall {
                    id: "todo-resume".into(),
                    name: "todo".into(),
                    input: Default::default(),
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "tool_use".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::ToolResult {
                id: new_id(),
                tool_use_id: "todo-resume".into(),
                content: "updated".into(),
                is_error: false,
                child_session_id: None,
                state: Some(ilar::session::SessionState::TodoList {
                    list: ilar::todo::TodoList {
                        items: vec![ilar::todo::TodoItem {
                            content: "restored".into(),
                            status: ilar::todo::Status::InProgress,
                        }],
                    },
                }),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);
        let resumed = store.load(&session_id).unwrap();

        let restored = restored_todos(Some(&resumed));
        assert_eq!(restored.items.len(), 1);
        assert_eq!(restored.items[0].content, "restored");
        assert_eq!(restored.items[0].status, ilar::todo::Status::InProgress);
    }

    #[test]
    fn notification_burst_stays_queued_while_a_turn_is_active() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        for description in ["first", "second"] {
            tx.try_send(ilar::subagent::Notification {
                parent_session_id: "parent".into(),
                description: description.into(),
                text: description.into(),
                is_error: false,
            })
            .unwrap();
        }

        let mut pending = None;
        // Blocked for any reason: a running turn, a pause, or an
        // overlay owning the keyboard. `may_route_notification` decides
        // which; this only checks the gate is honoured.
        assert!(next_notification(true, &mut pending, &mut rx).is_none());
        assert_eq!(rx.len(), 2, "a blocked gate must not consume the backlog");
        assert_eq!(
            next_notification(false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "first"
        );
        assert_eq!(
            next_notification(false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "second"
        );
    }

    #[test]
    fn propagated_notification_follows_the_existing_receiver_backlog() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        tx.try_send(ilar::subagent::Notification {
            parent_session_id: "parent".into(),
            description: "queued".into(),
            text: "queued".into(),
            is_error: false,
        })
        .unwrap();
        let mut pending = Some(PendingNotification {
            notification: ilar::subagent::Notification {
                parent_session_id: "parent".into(),
                description: "propagated".into(),
                text: "propagated".into(),
                is_error: false,
            },
            queued_ahead: rx.len(),
        });

        assert_eq!(
            next_notification(false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "queued"
        );
        assert_eq!(
            next_notification(false, &mut pending, &mut rx)
                .unwrap()
                .description,
            "propagated"
        );
    }
}
