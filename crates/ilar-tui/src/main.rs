//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod app;
mod decide;
mod diff;
mod exec;
mod highlight;
mod history;
mod input;
mod links;
mod markdown;
mod modals;
mod questions;
mod schedule;
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
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use decide::{Intent, LoopState, retry as retry_intents};
use input::{
    InputBuffer, Interrupt, PromptAction, handle_prompt_key, interrupt, quit_requested,
    retry_requested,
};
use modals::{
    CommandPaletteAction, Modal, ModelPicker, PendingAction, PendingManager, PickerAction,
    SessionPicker, SessionPickerAction, ThemePicker, TurnPicker, TurnPickerAction, VariantPicker,
    VariantPickerAction, is_command_palette_shortcut, turn_entries,
};
use questions::QuestionAction;
use ratatui::style::Color;
use sidebar::AgentRow;
use tokio_util::sync::CancellationToken;
use transcript::Line_;

use ilar::agent::{
    LOOP_EVENT_CAPACITY, LoopConfig, LoopEventReceiver, TurnOutcome, loop_event_channel, run_turn,
};
use ilar::config::Loader;
use ilar::provider::ProviderResolver;
use ilar::runtime::{ensure_direct_resume_allowed, persist_model_change};
use ilar::session::SessionStore;
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
    /// Run one turn without a terminal and print the answer
    Exec(ExecArgs),
}

#[derive(clap::Args, Debug)]
struct ExecArgs {
    /// The prompt. Omit to read it from stdin.
    prompt: Option<String>,

    /// Model to use (provider/model-id); overrides config.
    #[arg(long)]
    model: Option<String>,

    /// Agent name from config.
    #[arg(long)]
    agent: Option<String>,

    /// Session id to continue.
    #[arg(long)]
    session: Option<String>,

    /// Continue the most recently modified session.
    #[arg(long = "continue", conflicts_with = "session")]
    continue_last: bool,

    /// Emit the loop's events as NDJSON on stdout instead of the answer.
    #[arg(long)]
    json: bool,
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
    if let Some(("compact", args)) = parse_slash_invocation(&text) {
        if args.is_empty() {
            app.compact_requested = true;
            app.set_notice("compaction starting", NoticeLevel::Info);
        } else {
            app.input = InputBuffer::from(text.as_str());
            app.set_notice("usage: /compact", NoticeLevel::Warning);
        }
        return None;
    }
    if let Some(("rewind", args)) = parse_slash_invocation(&text) {
        if args.is_empty() {
            app.turn_picker_requested = true;
        } else {
            app.input = InputBuffer::from(text.as_str());
            app.set_notice("usage: /rewind", NoticeLevel::Warning);
        }
        return None;
    }
    if let Some(("fork", args)) = parse_slash_invocation(&text) {
        if args.is_empty() {
            app.fork_requested = true;
        } else {
            app.input = InputBuffer::from(text.as_str());
            app.set_notice(
                "usage: /fork — Ctrl-Y in the /rewind picker forks at a turn",
                NoticeLevel::Warning,
            );
        }
        return None;
    }
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
            SlashResolution::Prompt(expanded, overrides) => {
                return apply_command_overrides(app, name, &text, expanded, overrides);
            }
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

/// A command's frontmatter, applied. Returns the prompt for a plain or
/// model-overridden invocation; `None` when the command runs as a
/// subtask (armed on the app) or the overrides do not validate (the
/// input is restored for editing, like an unknown `/name`).
fn apply_command_overrides(
    app: &mut App,
    name: &str,
    typed: &str,
    expanded: String,
    overrides: CommandOverrides,
) -> Option<String> {
    // `agent` only means anything as a subagent type, so it implies
    // subtask; opencode's primary-agent switching has no counterpart
    // here.
    if overrides.subtask || overrides.agent.is_some() {
        app.pending_subtask = Some(crate::app::SubtaskRequest {
            description: format!("/{name}"),
            prompt: expanded,
            agent: overrides.agent.unwrap_or_else(|| "build".into()),
            model: overrides.model,
            variant: overrides.variant,
        });
        return None;
    }
    if overrides.model.is_none() && overrides.variant.is_none() {
        return Some(expanded);
    }
    // Validate at invocation, not load: a foreign command file with an
    // unknown model must still list, but must not silently run the
    // turn under the wrong model. Same available-set rule as the task
    // tool: an empty set (tests, bare configs) falls back to the
    // catalog.
    let target = overrides
        .model
        .clone()
        .unwrap_or_else(|| app.current_model.clone());
    let known = app.available_models.contains(&target)
        || (app.available_models.is_empty() && ilar::model::find(&target).is_some());
    if overrides.model.is_some() && !known {
        app.input = InputBuffer::from(typed);
        app.set_notice(
            format!("/{name}: unknown or unavailable model {target:?} — F2 lists them"),
            NoticeLevel::Warning,
        );
        return None;
    }
    if let Err(error) = ilar::model::variant_options(&target, overrides.variant.as_deref()) {
        app.input = InputBuffer::from(typed);
        app.set_notice(format!("/{name}: {error:#}"), NoticeLevel::Warning);
        return None;
    }
    app.pending_model_override = Some((overrides.model, overrides.variant));
    Some(expanded)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnRequest {
    New(String),
    Resume,
}

/// Apply one intent to the app. Returns the request when the intent starts a
/// turn — spawning needs the runtime, everything else does not.
fn apply_intent(
    app: &mut App,
    intent: Intent,
    steer: Option<&ilar::agent::SteerSender>,
) -> Option<TurnRequest> {
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
        Intent::PasteQuestion(text) => {
            if let Some(question) = app.question_modal.as_mut() {
                question.paste(&text);
            }
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
        Intent::ResumeTurn => {
            app.retry_available = false;
            // The failed turn already committed its chain. Keep that retry
            // disposition even if this resume fails before TurnStarted.
            app.turn_committed = true;
            app.clear_notice();
            app.follow_tail = true;
            app.busy = true;
            app.status = "thinking".into();
            app.set_activity(Activity::Thinking);
            Some(TurnRequest::Resume)
        }
        Intent::StartTurn(text) => {
            // Starting a new turn preserves the raw text only in prompt history;
            // failed-turn resume uses persisted conversation state.
            let text = prepare_prompt(app, text)?;
            app.retry_available = false;
            app.turn_committed = false;
            app.clear_notice();
            app.push_transcript_line(Line_::User(text.clone()));
            app.follow_tail = true;
            app.busy = true;
            app.status = "thinking".into();
            app.set_activity(Activity::Thinking);
            Some(TurnRequest::New(text))
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
            Intent::StartTurn(_) | Intent::ResumeTurn => deferred.push(intent),
            other => {
                let started = apply_intent(app, other, steer);
                debug_assert!(started.is_none(), "only turn intents yield a request");
            }
        }
    }
}

/// Rounds after which goal mode gives up (a budget, unlike the
/// runaway-loop iteration guard).
const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("goal", "work until the goal is achieved (evidence-based)"),
    (
        "compact",
        "compact the session now and show its handover summary",
    ),
    (
        "rewind",
        "pick a turn to rewind conversation and tree to (^Y forks instead)",
    ),
    ("fork", "fork this session under a new id"),
];
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

/// The frontmatter a command carries into its invocation — see
/// meta/issues/honour-command-frontmatter.md for the semantics.
#[derive(Debug, PartialEq, Default, Clone)]
struct CommandOverrides {
    model: Option<String>,
    variant: Option<String>,
    agent: Option<String>,
    subtask: bool,
}

/// What `/name args` means. Extracted from the key handler so the
/// precedence between commands, skills and the built-in is testable
/// without a running event loop.
#[derive(Debug, PartialEq)]
enum SlashResolution {
    /// A command's body, arguments already substituted.
    Prompt(String, CommandOverrides),
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
        return SlashResolution::Prompt(
            expanded,
            CommandOverrides {
                model: command.model.clone(),
                variant: command.variant.clone(),
                agent: command.agent.clone(),
                subtask: command.subtask,
            },
        );
    }
    if app.skills.iter().any(|(skill, _)| skill == name) {
        return SlashResolution::Skill(skill_invocation_prompt(name, args));
    }
    let mut inventory = app.slash_inventory();
    inventory.retain(|(name, _)| {
        !BUILTIN_SLASH_COMMANDS
            .iter()
            .any(|(builtin, _)| name == builtin)
    });
    inventory.extend(
        BUILTIN_SLASH_COMMANDS
            .iter()
            .map(|(name, description)| ((*name).into(), (*description).into())),
    );
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

/// `ilar exec`: resolve the same runtime the TUI would, run one turn,
/// and return the exit code the shell should see.
async fn run_exec(config: &ilar::config::Config, args: ExecArgs) -> Result<i32> {
    use std::io::Write as _;
    let prompt = match args.prompt {
        Some(prompt) => prompt,
        None => {
            let mut buffer = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
                .context("reading the prompt from stdin")?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        anyhow::bail!("no prompt given (pass one as an argument or on stdin)");
    }

    let store = ilar::runtime::session_store(config);
    let resume = if args.continue_last {
        Some(
            store
                .latest()
                .map(|session| session.id)
                .context("no sessions to continue (session directory is empty)")?,
        )
    } else {
        args.session
    };
    let runtime = ilar::runtime::RuntimePlan::resolve(
        config,
        &ilar::runtime::RuntimeOptions {
            model: args.model,
            agent: args.agent,
            resume,
            cwd: std::env::current_dir().context("no cwd")?,
            // Nobody is here to answer: the tool is left off so the
            // model is told so on the spot instead of blocking.
            questions: false,
        },
    )?
    .start(config)?;

    let format = if args.json {
        exec::ExecFormat::Json
    } else {
        exec::ExecFormat::Text
    };
    let cancel = CancellationToken::new();
    let interrupt = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupt.cancel();
        }
    });

    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let outcome = exec::exec_turn(
        runtime.resolver.as_ref(),
        &runtime.registry,
        &runtime.store,
        &runtime.session_id,
        prompt.trim(),
        Some(&runtime.system_prompt),
        runtime.loop_config.clone(),
        runtime.tool_ctx.clone(),
        format,
        cancel,
        &mut out,
        &mut err,
    )
    .await;

    // Nothing outlives the process: a background task with no session
    // to notify, or a service nobody will stop, is a leak.
    let background = runtime.spawner.running_background();
    if background > 0 {
        let _ = writeln!(err, "{background} background task(s) cancelled at exit");
    }
    runtime.spawner.shutdown().await;
    runtime.services.stop_all();

    if let Err(error) = &outcome {
        let _ = writeln!(err, "error: {error:#}");
    }
    Ok(exec::exit_code(&outcome))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Loader::new().resolve().context("loading config")?;
    if let Some(Command::Exec(exec_args)) = args.command {
        let code = run_exec(&config, exec_args).await?;
        std::process::exit(code);
    }
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
    // Prefill + notice carried across a rewind/fork rebuild.
    let mut carried: Option<(Option<String>, Option<String>)> = None;
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

        let cwd = std::env::current_dir().context("no cwd")?;
        let plan = ilar::runtime::RuntimePlan::resolve(
            &config,
            &ilar::runtime::RuntimeOptions {
                model: cli_model.map(str::to_string),
                agent: cli_agent.map(str::to_string),
                resume: resume_target.clone(),
                cwd: cwd.clone(),
                questions: true,
            },
        )?;
        if args.print_prompt {
            println!("{}", plan.system_prompt);
            return Ok(());
        }
        let model_for_session = plan.model.clone();
        let skill_inventory = plan.skills.clone();
        let command_inventory = plan.commands.clone();
        let system_prompt = plan.system_prompt.clone();
        let runtime = plan.start(&config)?;
        let ilar::runtime::SessionRuntime {
            store: _,
            session_id,
            reasoning: reasoning_for_session,
            registry,
            spawner,
            services,
            todos,
            tool_ctx,
            loop_config,
            resolver,
            questions,
            resumed,
            ..
        } = runtime;
        let question_rx = questions.expect("the TUI asked for questions");
        let notifications = spawner.subscribe();
        let subagent_activity = spawner.subscribe_activity();
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
        app.available_models = model_choices.iter().map(|model| model.full_id()).collect();
        app.session_id = session_id.clone();
        app.todos = todos;
        if let Some(resumed) = &resumed {
            app.restore_session(resumed, &store);
            if let Some(pending) = resumed.pending_question() {
                app.question_modal = Some(questions::QuestionModal::new(pending.request.clone()));
                app.busy = true;
                app.status = "waiting for your answer".into();
                app.set_activity(Activity::Paused);
            }
        }
        app.configure_runtime(
            model_for_session.clone(),
            reasoning_for_session,
            cwd.clone(),
            context_used,
            context_limit,
            context_estimated,
        );
        if app.question_modal.is_some() {
            app.status = "waiting for your answer".into();
        }
        if let Some((prefill, notice)) = carried.take() {
            if let Some(prefill) = prefill {
                app.input = InputBuffer::from(prefill.as_str());
            }
            if let Some(notice) = notice {
                app.set_notice(notice, NoticeLevel::Info);
            }
        }

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
            question_rx,
            loop_config,
            model_choices,
            services,
        )
        .await?;
        active_theme = app.theme;
        match exit {
            AppExit::Quit => return Ok(()),
            AppExit::Switch(next) => session_override = Some(next),
            AppExit::SwitchInto {
                id,
                prefill,
                notice,
            } => {
                session_override = Some(id);
                carried = Some((prefill, notice));
            }
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
    Compaction(Result<ilar::compaction::ManualCompactionOutcome>),
}

/// `settle`'s effectful edges over tokio and crossterm — see
/// schedule.rs for the order it is driven in. Constructed fresh each
/// iteration around mutable borrows of the loop's state, so a spawn
/// here is a spawn the rest of the iteration observes.
struct LoopRuntime<'a> {
    turn_handle: &'a mut Option<tokio::task::JoinHandle<TurnCompletion>>,
    events_rx: &'a mut Option<LoopEventReceiver>,
    cancel: &'a mut Option<CancellationToken>,
    steer_tx: &'a mut Option<ilar::agent::SteerSender>,
    pending_terminal_event: &'a mut Option<Event>,
    pending_notification: &'a mut Option<PendingNotification>,
    notifications: &'a mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    ring_on_turn_completion: &'a mut bool,
    notifications_paused: &'a mut bool,
    resolver: &'a Arc<dyn ProviderResolver>,
    store: &'a SessionStore,
    session_id: &'a str,
    system_prompt: &'a str,
    registry: &'a ToolRegistry,
    tool_ctx: &'a ToolContext,
    loop_config: &'a LoopConfig,
    spawner: &'a std::sync::Arc<ilar::subagent::SubagentSpawner>,
    services: &'a std::sync::Arc<ilar::tools::service::ServiceManager>,
    terminal: &'a mut ratatui::DefaultTerminal,
    bell_pending: &'a mut bool,
}

impl schedule::Runtime for LoopRuntime<'_> {
    fn observe(&self, app: &App) -> LoopState {
        observe(
            app,
            self.turn_handle,
            self.pending_terminal_event,
            self.steer_tx,
            *self.notifications_paused,
        )
    }

    fn perform(&mut self, app: &mut App, intent: Intent) -> Result<()> {
        let Some(request) = apply_intent(app, intent, self.steer_tx.as_ref()) else {
            return Ok(());
        };
        // Every push site is guarded against starting a turn while
        // one runs. If that ever slips, the running turn would be
        // orphaned — its cancellation token dropped without firing,
        // still writing to the same session.
        debug_assert!(
            self.turn_handle.is_none(),
            "starting a turn while one is already running"
        );
        // A command's one-turn model override, validated by
        // prepare_prompt; the provider check happens in the adopt.
        // On failure the turn still runs — its prompt is already in
        // the transcript — just under the session's own model.
        if matches!(&request, TurnRequest::New(_))
            && let Some((model, variant)) = app.pending_model_override.take()
        {
            let target = model.unwrap_or_else(|| app.current_model.clone());
            let revert = (app.current_model.clone(), app.current_variant.clone());
            match adopt_model_selection(
                app,
                self.resolver.as_ref(),
                self.store,
                self.session_id,
                self.system_prompt,
                self.registry,
                target,
                variant,
            ) {
                Ok(()) => app.model_revert = Some(revert),
                Err(error) => app.set_notice(
                    format!(
                        "command model override failed ({error:#}) — running with {}",
                        revert.0
                    ),
                    NoticeLevel::Warning,
                ),
            }
        }
        let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        *self.events_rx = Some(rx);
        let token = CancellationToken::new();
        *self.cancel = Some(token.clone());
        let resolver = self.resolver.clone();
        let store = self.store.clone();
        let session_id = self.session_id.to_string();
        let system_prompt = self.system_prompt.to_string();
        let registry = self.registry.clone();
        let turn_ctx = self.tool_ctx.clone();
        let loop_config = self.loop_config.clone();
        *self.ring_on_turn_completion = true;
        let (tx_steer, steer_rx) = ilar::agent::steer_channel();
        *self.steer_tx = Some(tx_steer);
        *self.turn_handle = Some(tokio::spawn(async move {
            let result = match request {
                TurnRequest::New(text) => {
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
                    .await
                }
                TurnRequest::Resume => {
                    ilar::agent::resume_turn(
                        resolver.as_ref(),
                        &registry,
                        &store,
                        &session_id,
                        Some(&system_prompt),
                        loop_config,
                        tx,
                        token,
                        turn_ctx,
                        Some(steer_rx),
                    )
                    .await
                }
            };
            TurnCompletion::Root(result)
        }));
        Ok(())
    }

    fn peek_palette(&mut self, app: &mut App) -> Result<()> {
        // Let a buffered Ctrl-P open the palette before the gate can
        // hand the keyboard to a notification turn.
        let modal_open = app.has_modal();
        if self.turn_handle.is_none()
            && !*self.notifications_paused
            && !modal_open
            && self.pending_terminal_event.is_none()
            && crossterm::event::poll(std::time::Duration::ZERO)?
        {
            *self.pending_terminal_event = Some(crossterm::event::read()?);
        }
        if self
            .pending_terminal_event
            .as_ref()
            .is_some_and(is_command_palette_shortcut)
        {
            *self.pending_terminal_event = None;
            app.model_key_pending = false;
            if app.search_active {
                app.close_search(false);
            }
            app.open_command_palette();
        }
        Ok(())
    }

    fn next_notification(&mut self, held: bool) -> Option<ilar::subagent::Notification> {
        next_notification(held, self.pending_notification, self.notifications)
    }

    fn start_compaction(&mut self, app: &mut App) {
        debug_assert!(self.turn_handle.is_none());
        app.busy = true;
        app.status = "compacting session".into();
        app.clear_transient_notice();
        app.set_activity(Activity::Thinking);

        let token = CancellationToken::new();
        *self.cancel = Some(token.clone());
        let resolver = self.resolver.clone();
        let store = self.store.clone();
        let session_id = self.session_id.to_string();
        let system_prompt = self.system_prompt.to_string();
        let registry = self.registry.clone();
        *self.turn_handle = Some(tokio::spawn(async move {
            let tools = registry.definitions();
            let result = ilar::compaction::compact_session(
                resolver.as_ref(),
                &store,
                &session_id,
                Some(&system_prompt),
                &tools,
                &token,
            )
            .await;
            TurnCompletion::Compaction(result)
        }));
    }

    fn route(&mut self, app: &mut App, notification: ilar::subagent::Notification) {
        let token = CancellationToken::new();
        *self.cancel = Some(token.clone());
        app.busy = true;
        app.status = format!("routing task to {}", notification.parent_session_id);
        app.clear_transient_notice();
        app.set_activity(Activity::Thinking);
        let spawner = self.spawner.clone();
        *self.turn_handle = Some(tokio::spawn(async move {
            TurnCompletion::Routed(spawner.route_notification(notification, token).await)
        }));
    }

    fn start_notification_turn(
        &mut self,
        app: &mut App,
        notification: ilar::subagent::Notification,
    ) {
        let text = notification.text;
        app.push_notification(&notification.description, &text);
        let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
        *self.events_rx = Some(rx);
        let token = CancellationToken::new();
        *self.cancel = Some(token.clone());
        app.busy = true;
        app.turn_committed = false;
        app.retry_available = false;
        app.status = "thinking".into();
        app.clear_transient_notice();
        app.set_activity(Activity::Thinking);
        let resolver = self.resolver.clone();
        let store = self.store.clone();
        let session_id = self.session_id.to_string();
        let system_prompt = self.system_prompt.to_string();
        let registry = self.registry.clone();
        let turn_ctx = self.tool_ctx.clone();
        let loop_config = self.loop_config.clone();
        let (tx_steer, steer_rx) = ilar::agent::steer_channel();
        *self.steer_tx = Some(tx_steer);
        *self.turn_handle = Some(tokio::spawn(async move {
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

    fn session_id(&self) -> &str {
        self.session_id
    }

    fn resume_notifications(&mut self) {
        *self.notifications_paused = false;
    }

    fn pause_notifications(&mut self) {
        *self.notifications_paused = true;
    }

    fn hold_propagate(&mut self, notification: ilar::subagent::Notification) {
        *self.pending_notification = Some(PendingNotification {
            notification,
            queued_ahead: self.notifications.len(),
        });
    }

    fn hold_requeue(&mut self, notification: ilar::subagent::Notification) {
        *self.pending_notification = Some(PendingNotification {
            notification,
            queued_ahead: 0,
        });
    }

    fn end_turn(&mut self) {
        *self.events_rx = None;
        *self.cancel = None;
        *self.steer_tx = None;
    }

    fn revert_model(&mut self, app: &mut App, model: String, variant: Option<String>) {
        // Hand-rolled rather than through adopt_model_selection
        // because a revert must not clear an error notice the turn
        // just set.
        match persist_model_change(
            self.resolver.as_ref(),
            self.store,
            self.session_id,
            &model,
            variant.as_deref(),
        ) {
            Ok(()) => {
                app.current_model = model.clone();
                app.current_variant = variant;
                app.context_limit = display_context_limit(self.resolver.as_ref(), &model);
                app.push_transcript_line(Line_::System(format!("model reverted to {model}")));
            }
            Err(error) => app.set_notice(
                format!("reverting the model to {model} failed: {error:#}"),
                NoticeLevel::Error,
            ),
        }
    }

    async fn start_subtask(&mut self, app: &mut App, request: crate::app::SubtaskRequest) {
        let description = request.description.clone();
        // The root ToolContext carries no session id — run_turn fills
        // it per turn, and this call bypasses run_turn. An empty id
        // here creates an unroutable completion notification that
        // wedges the pipeline.
        let mut task_ctx = self.tool_ctx.clone();
        task_ctx.session_id = self.session_id.to_string();
        let output = self
            .spawner
            .run_task(
                ilar::subagent::TaskInput {
                    description: request.description,
                    prompt: request.prompt,
                    subagent_type: request.agent.clone(),
                    task_id: None,
                    background: Some(true),
                    workspace: None,
                    model: request.model,
                    reasoning: request.variant,
                },
                &task_ctx,
            )
            .await;
        if output.is_error {
            app.set_notice(
                format!("{description}: {}", output.content),
                NoticeLevel::Error,
            );
            app.push_transcript_line(Line_::System(format!(
                "{description} failed to start: {}",
                output.content
            )));
        } else {
            let line = format!(
                "{description} running in the background as {} — completion arrives as a notification",
                request.agent
            );
            app.set_notice(&line, NoticeLevel::Info);
            app.push_transcript_line(Line_::System(line));
        }
    }

    fn present(&mut self, app: &mut App) -> Result<()> {
        let _ = ring_terminal_bell_if_idle(
            &mut std::io::stdout(),
            self.bell_pending,
            self.turn_handle.is_some(),
        );
        app.background_running = self.spawner.running_background();
        app.agents_view = self
            .spawner
            .running_tasks()
            .into_iter()
            .map(|task| AgentRow {
                description: task.description,
                agent: task.agent,
                background: task.background,
                elapsed: task.started.elapsed(),
            })
            .collect();
        app.services_view = self.services.snapshot();
        app.services_running = app
            .services_view
            .iter()
            .filter(|(_, running, _)| *running)
            .count();
        self.terminal.draw(|frame| app.render(frame))?;
        Ok(())
    }

    fn poll_event(&mut self, busy: bool) -> Result<Option<Event>> {
        if let Some(event) = self.pending_terminal_event.take() {
            return Ok(Some(event));
        }
        // Fast while busy so streaming keeps rendering.
        let timeout = if busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        if !crossterm::event::poll(timeout)? {
            return Ok(None);
        }
        Ok(Some(crossterm::event::read()?))
    }
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

/// The reasons a session switch (resume, fork, rewind) must wait; the
/// same triple guards every path that tears the runtime down.
fn switch_blocked(
    turn_running: bool,
    background_agents: usize,
    has_draft: bool,
) -> Option<&'static str> {
    if turn_running {
        Some("finish or abort the current turn before switching sessions")
    } else if background_agents > 0 {
        Some("background agents are running; wait or abort them first")
    } else if has_draft {
        Some("input has an unsent draft; send or clear it first")
    } else {
        None
    }
}

/// How run_app ended: quit the program, or restart against another session.
enum AppExit {
    Quit,
    Switch(String),
    /// Switch carrying rewind/fork context into the rebuilt app: an
    /// input prefill (the unsent message) and a notice.
    SwitchInto {
        id: String,
        prefill: Option<String>,
        notice: Option<String>,
    },
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
    mut question_rx: ilar::question::QuestionReceiver,
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
    let mut question_reply: Option<tokio::sync::oneshot::Sender<ilar::question::QuestionResponse>> =
        None;
    let mut pending_question_id = store
        .load(session_id)?
        .pending_question()
        .map(|pending| pending.tool_call_id.clone());
    // Decisions accumulate here and are performed in one place below,
    // rather than each arm doing its own effects inline.
    let mut intents: Vec<Intent> = Vec::new();

    loop {
        // A failed/cancelled resume may leave the persisted question pending.
        // Reopen it instead of stranding the session behind a rejected new turn.
        if turn_handle.is_none()
            && app.question_modal.is_none()
            && let Some(pending) = store.load(session_id)?.pending_question()
        {
            pending_question_id = Some(pending.tool_call_id.clone());
            question_reply = None;
            app.question_modal = Some(questions::QuestionModal::new(pending.request.clone()));
            app.busy = true;
            app.status = "waiting for your answer".into();
            app.set_activity(Activity::Paused);
        }
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
        // Questions use their own typed reply path and intentionally wait
        // outside the ordinary tool executor. Apply them after loop events so
        // the waiting state wins over the preceding StepComplete update.
        while let Ok(prompt) = question_rx.try_recv() {
            if prompt.session_id != session_id {
                continue;
            }
            pending_question_id = Some(prompt.tool_call_id);
            question_reply = Some(prompt.reply);
            app.question_modal = Some(questions::QuestionModal::new(prompt.request));
            app.status = "waiting for your answer".into();
            app.set_activity(Activity::Paused);
        }
        // Rewind and fork requests recorded by /rewind, /fork or the
        // palette; they need the store, so they are consumed here.
        if std::mem::take(&mut app.turn_picker_requested) {
            if let Some(reason) = switch_blocked(
                turn_handle.is_some(),
                spawner.running_background(),
                !app.input.is_blank(),
            ) {
                app.set_notice(reason, NoticeLevel::Warning);
            } else if app.goal.is_some() {
                app.set_notice(
                    "a goal is active — /goal abort before rewinding away its context",
                    NoticeLevel::Warning,
                );
            } else {
                match store.load(session_id) {
                    Ok(reader) => {
                        app.turn_picker = Some(TurnPicker::new(turn_entries(reader.events())));
                    }
                    Err(error) => {
                        app.set_notice(format!("cannot load session: {error}"), NoticeLevel::Error);
                    }
                }
            }
        }
        if std::mem::take(&mut app.fork_requested) {
            if let Some(reason) = switch_blocked(
                turn_handle.is_some(),
                spawner.running_background(),
                !app.input.is_blank(),
            ) {
                app.set_notice(reason, NoticeLevel::Warning);
            } else {
                match store.fork(session_id) {
                    Ok(fork_id) => {
                        spawner.shutdown().await;
                        return Ok(AppExit::SwitchInto {
                            id: fork_id,
                            prefill: None,
                            notice: Some(format!("forked from {session_id}")),
                        });
                    }
                    Err(error) => {
                        app.set_notice(format!("cannot fork: {error}"), NoticeLevel::Error);
                    }
                }
            }
        }
        // Turn finished? Join at the edge; schedule::pass folds the
        // completion into the same pass as the drain and the gate.
        let mut completion = None;
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
            completion = Some(match handle.await {
                Ok(TurnCompletion::Root(result)) => schedule::Completion::Root(result),
                Ok(TurnCompletion::Routed(result)) => schedule::Completion::Routed(result),
                Ok(TurnCompletion::Compaction(result)) => schedule::Completion::Compaction(result),
                Err(error) => schedule::Completion::Crashed(error.to_string()),
            });
        }

        // The whole iteration minus the dispatch — completion
        // bookkeeping and its after_turn decisions, the intent drain,
        // the palette peek, the notification gate, the subtask spawn,
        // the frame and the poll — lives in schedule::tick so tests
        // can drive the sequence. Only the effects live here.
        let outcome = schedule::tick(
            app,
            completion,
            std::mem::take(&mut intents),
            &mut LoopRuntime {
                turn_handle: &mut turn_handle,
                events_rx: &mut events_rx,
                cancel: &mut cancel,
                steer_tx: &mut steer_tx,
                pending_terminal_event: &mut pending_terminal_event,
                pending_notification: &mut pending_notification,
                notifications: &mut notifications,
                ring_on_turn_completion: &mut ring_on_turn_completion,
                notifications_paused: &mut notifications_paused,
                resolver: &resolver,
                store,
                session_id,
                system_prompt,
                registry,
                tool_ctx: &tool_ctx,
                loop_config: &loop_config,
                spawner: &spawner,
                services: &services,
                terminal: &mut *terminal,
                bell_pending: &mut bell_pending,
            },
        )
        .await?;
        let event = match outcome {
            schedule::Tick::Restart | schedule::Tick::Idle => continue,
            schedule::Tick::Dispatch(event) => event,
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
                let alt = modifiers.contains(KeyModifiers::ALT);
                // Ctrl-C is an interrupt, not the exit: it is rewritten
                // into Esc and rides the dispatch below, so every scope
                // keeps exactly one set of dismiss/abort/clear paths.
                let (key, code) = if matches!((code, control), (KeyCode::Char('c'), true)) {
                    match interrupt(
                        app.has_modal() || app.model_key_pending,
                        app.busy,
                        app.input.is_blank(),
                    ) {
                        Interrupt::Hint => {
                            app.set_notice(
                                "nothing to interrupt — Ctrl-D on a blank prompt quits",
                                NoticeLevel::Info,
                            );
                            continue;
                        }
                        Interrupt::AsEsc => (
                            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                            KeyCode::Esc,
                        ),
                    }
                } else {
                    (key, code)
                };
                // The exit, EOF-style: a blank prompt with nothing open.
                if quit_requested(code, control, app.has_modal(), app.input.is_blank()) {
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                    spawner.shutdown().await;
                    return Ok(AppExit::Quit);
                }
                // One exhaustive match over the active overlay: adding a
                // `Modal` variant without a dispatch arm is a compile
                // error, which the old `if` chain could not promise.
                if let Some(modal) = app.active_modal() {
                    match modal {
                        Modal::Question => {
                            let action = app
                                .question_modal
                                .as_mut()
                                .expect("question modal")
                                .handle_key(key);
                            if let QuestionAction::Complete(response) = action {
                                app.question_modal = None;
                                app.status = "processing answer".into();
                                app.set_activity(Activity::Tools);
                                if let Some(reply) = question_reply.take() {
                                    let _ = reply.send(response);
                                    pending_question_id = None;
                                } else if pending_question_id.take().is_some() {
                                    app.turn_committed = false;
                                    app.retry_available = false;
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
                                    let loop_config = loop_config.clone();
                                    let (tx_steer, steer_rx) = ilar::agent::steer_channel();
                                    steer_tx = Some(tx_steer);
                                    ring_on_turn_completion = true;
                                    turn_handle = Some(tokio::spawn(async move {
                                        TurnCompletion::Root(
                                            ilar::agent::resume_pending_question(
                                                resolver.as_ref(),
                                                &registry,
                                                &store,
                                                &session_id,
                                                response,
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
                            }
                        }
                        Modal::PendingManager => match app.pending_manager_key(code, control) {
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
                                    let decided = retry_intents(&state);
                                    if decided.iter().any(|i| matches!(i, Intent::StartTurn(_))) {
                                        app.pending_manager = None;
                                    }
                                    intents.extend(decided);
                                }
                            }
                        },
                        Modal::Help => match code {
                            KeyCode::Up => app.help_scroll = app.help_scroll.saturating_sub(1),
                            KeyCode::Down => app.help_scroll = app.help_scroll.saturating_add(1),
                            KeyCode::PageUp => {
                                app.help_scroll = app.help_scroll.saturating_sub(10);
                            }
                            KeyCode::PageDown => {
                                app.help_scroll = app.help_scroll.saturating_add(10);
                            }
                            KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?' | 'q') => {
                                app.help_visible = false;
                                app.help_scroll = 0;
                            }
                            _ => {}
                        },
                        Modal::Todos => match (code, control) {
                            (KeyCode::Up, _) => {
                                app.todos_scroll = app.todos_scroll.saturating_sub(1);
                            }
                            (KeyCode::Down, _) => {
                                app.todos_scroll = app.todos_scroll.saturating_add(1);
                            }
                            (KeyCode::PageUp, _) => {
                                app.todos_scroll = app.todos_scroll.saturating_sub(10);
                            }
                            (KeyCode::PageDown, _) => {
                                app.todos_scroll = app.todos_scroll.saturating_add(10);
                            }
                            (KeyCode::Esc | KeyCode::Char('q'), false)
                            | (KeyCode::Char('t'), true) => {
                                app.todos_visible = false;
                                app.todos_scroll = 0;
                            }
                            _ => {}
                        },
                        Modal::ThemePicker => {
                            let action = {
                                let picker = app.theme_picker.as_mut().unwrap();
                                picker.handle_key(code, control)
                            };
                            apply_theme_picker_action(app, action, |selected| {
                                ilar::config::persist_general_theme(user_config_path, selected.id())
                            });
                        }
                        Modal::SkillPicker => {
                            let picker = app.skill_picker.as_mut().unwrap();
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
                        }
                        Modal::SessionPicker => {
                            let picker = app.session_picker.as_mut().unwrap();
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
                                            // Through the hook, not the field:
                                            // select() also disarms.
                                            picker.select(0);
                                        }
                                        app.set_notice(
                                            format!("deleted session {id}"),
                                            NoticeLevel::Info,
                                        );
                                    }
                                    Err(error) => {
                                        app.set_notice(
                                            format!("cannot delete {id}: {error}"),
                                            NoticeLevel::Error,
                                        );
                                    }
                                },
                                SessionPickerAction::Fork(id) => {
                                    let blocked = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        !app.input.is_blank(),
                                    );
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
                                    let blocked = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        !app.input.is_blank(),
                                    );
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
                        }
                        Modal::TurnPicker => {
                            let picker = app.turn_picker.as_mut().unwrap();
                            match picker.handle_key(code, control) {
                                TurnPickerAction::Stay => {}
                                TurnPickerAction::Dismiss => {
                                    app.turn_picker = None;
                                    app.clear_transient_notice();
                                }
                                TurnPickerAction::Rewind {
                                    cut,
                                    target,
                                    discarded,
                                } => {
                                    if let Some(reason) = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        false,
                                    ) {
                                        app.set_notice(reason, NoticeLevel::Warning);
                                        continue;
                                    }
                                    app.turn_picker = None;
                                    match ilar::rewind::rewind_session(
                                        store,
                                        session_id,
                                        cut,
                                        &target,
                                        &tool_ctx.cwd,
                                    )
                                    .await
                                    {
                                        Ok(report) => {
                                            spawner.shutdown().await;
                                            let mut notice = if report.tree_restored {
                                                format!(
                                                    "rewound {discarded} turn(s) · tree restored"
                                                )
                                            } else {
                                                format!(
                                                    "rewound {discarded} turn(s) · no tree snapshot"
                                                )
                                            };
                                            if report.head_moved {
                                                notice
                                                    .push_str(" · HEAD moved since (commits kept)");
                                            }
                                            return Ok(AppExit::SwitchInto {
                                                id: session_id.to_string(),
                                                prefill: Some(report.unsent),
                                                notice: Some(notice),
                                            });
                                        }
                                        Err(error) => {
                                            app.set_notice(
                                                format!("rewind failed: {error}"),
                                                NoticeLevel::Error,
                                            );
                                        }
                                    }
                                }
                                TurnPickerAction::Fork { cut, target } => {
                                    if let Some(reason) = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        false,
                                    ) {
                                        app.set_notice(reason, NoticeLevel::Warning);
                                        continue;
                                    }
                                    // The forked message is unsent in the copy;
                                    // this load both fetches it for the input
                                    // prefill and verifies the picker's target
                                    // still sits at `cut`.
                                    let unsent =
                                        store.load(session_id).ok().and_then(|reader| match reader
                                            .events()
                                            .get(cut)
                                        {
                                            Some(ilar::session::SessionEvent::UserMessage {
                                                id,
                                                text,
                                                ..
                                            }) if *id == target => Some(text.clone()),
                                            _ => None,
                                        });
                                    if unsent.is_none() {
                                        app.set_notice(
                                            "the session changed since the turn was chosen; reopen the picker",
                                            NoticeLevel::Warning,
                                        );
                                        app.turn_picker = None;
                                        continue;
                                    }
                                    app.turn_picker = None;
                                    match store.fork_at(session_id, cut) {
                                        Ok(fork_id) => {
                                            spawner.shutdown().await;
                                            return Ok(AppExit::SwitchInto {
                                                id: fork_id,
                                                prefill: unsent,
                                                notice: Some(format!(
                                                    "forked at that turn from {session_id}"
                                                )),
                                            });
                                        }
                                        Err(error) => {
                                            app.set_notice(
                                                format!("cannot fork here: {error}"),
                                                NoticeLevel::Error,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Modal::LinkPicker => {
                            let picker = app.link_picker.as_mut().unwrap();
                            match picker.handle_key(code, control) {
                                PickerAction::Stay => {}
                                PickerAction::Dismiss => {
                                    app.link_picker = None;
                                }
                                PickerAction::Choose(url) => {
                                    app.link_picker = None;
                                    match links::open_in_browser(&url) {
                                        Ok(()) => app
                                            .set_notice(format!("opened {url}"), NoticeLevel::Info),
                                        Err(error) => app.set_notice(
                                            format!("cannot open link: {error}"),
                                            NoticeLevel::Error,
                                        ),
                                    }
                                }
                            }
                        }
                        Modal::ModelPicker => {
                            let picker = app.model_picker.as_mut().unwrap();
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
                                                picker.error = Some(format!(
                                                    "cannot switch to {new_model}: {error}"
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Modal::VariantPicker => {
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
                        }
                        Modal::Search if modals::nav_delta(code, control).is_some() => {
                            let delta = modals::nav_delta(code, control).expect("guard");
                            app.search_jump(delta);
                        }
                        Modal::Search => match (code, control) {
                            (KeyCode::Esc, _) => app.close_search(true),
                            (KeyCode::Enter, _) => app.close_search(false),
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
                        },
                        Modal::CommandPalette => {
                            let palette = app.command_palette.as_mut().unwrap();
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
                    // The visible completion popup owns arrows before
                    // history recall or transcript scrolling can consume them.
                    _ if app.handle_prompt_navigation_key(key) => {}
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
                        // manager (Ctrl-Q) and explicit commands. Ctrl-C
                        // arrives here too — it is rewritten into Esc above.
                        if app.busy {
                            if let Some(cancel) = &cancel {
                                cancel.cancel();
                                app.status = "aborting…".into();
                                app.set_notice("aborting current operation…", NoticeLevel::Warning);
                                app.set_activity(Activity::Aborting);
                            }
                        } else if !app.input.is_blank() {
                            app.input.clear();
                        }
                    }
                    // Half-page scrolling lives on Alt so that Ctrl-U and
                    // Ctrl-D can keep one meaning each: kill to line
                    // start, and quit.
                    (KeyCode::Char('u' | 'U'), _) if alt => {
                        app.scroll_up(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Char('d' | 'D'), _) if alt => {
                        app.scroll_down(app.page_size().div_ceil(2));
                    }
                    (KeyCode::Home, true) => app.scroll_to_top(),
                    (KeyCode::End, true) => app.scroll_to_tail(),
                    (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                    (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                    // Prompt arrows were routed above; transcript paging
                    // stays on PgUp/wheel/^U.
                    (KeyCode::Char('f'), true) => {
                        app.open_search();
                    }
                    (KeyCode::Char('q'), true) => {
                        app.pending_manager = Some(PendingManager::default());
                    }
                    (KeyCode::Char('o'), true) => {
                        app.open_link_picker();
                    }
                    // Read-only, like the link picker: no busy guard.
                    (KeyCode::Char('t'), true) => {
                        app.todos_visible = true;
                        app.todos_scroll = 0;
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
                        intents.extend(retry_intents(&state));
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
            // A modal in front owns the mouse: a click on one of its
            // rows selects that row, anywhere else is consumed.
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) if app
                .active_modal()
                .is_some_and(|modal| modal != Modal::Search) =>
            {
                app.click_active_modal(column, row);
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
    use ilar::runtime::{create_root_session, restored_todos};
    use ilar::session::{SessionMeta, new_id};

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
    fn new_session_persists_configured_reasoning_before_startup_continues() {
        let root = std::env::temp_dir().join(format!("ilar-tui-reasoning-{}", new_id()));
        let store = SessionStore::new(root.clone());
        let session_id = new_id();

        create_root_session(
            &store,
            SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "openai/gpt-5.2".into(),
                workspace: None,
            },
            Some("high"),
        )
        .unwrap();

        assert_eq!(
            store.load(&session_id).unwrap().effective_variant(),
            Some("high".into())
        );

        let invalid_id = new_id();
        let error = create_root_session(
            &store,
            SessionMeta {
                session_id: invalid_id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
            },
            Some("high"),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unsupported variant"),
            "{error:#}"
        );
        assert!(
            store.load(&invalid_id).is_err(),
            "invalid reasoning created a session"
        );
        std::fs::remove_dir_all(root).unwrap();
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
                    item_id: None,
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
