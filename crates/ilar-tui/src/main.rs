//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod app;
mod decide;
mod diff;
mod exec;
mod grants;
mod highlight;
mod history;
mod input;
mod links;
mod markdown;
mod modals;
mod questions;
mod schedule;
mod secret_cli;
mod selection;
#[cfg(feature = "serve")]
mod serve;
mod session_view;
mod sidebar;
mod text;
mod theme;
mod transcript;
mod view;
mod watch;

use std::sync::Arc;

use anyhow::{Context, Result};
use app::{
    App, FocusCancel, FocusView, activate_palette_command, apply_context_picker_action,
    apply_theme_picker_action,
};
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::supports_keyboard_enhancement;
use decide::{Intent, LoopState, retry as retry_intents, retry_dismisses_manager};
use grants::{GrantAction, PasswordAction};
use input::{
    InputBuffer, Interrupt, PromptAction, handle_prompt_key, interrupt, quit_requested,
    retry_requested,
};
use modals::{
    CommandPaletteAction, MAX_SEARCH_ROWS, Modal, ModelPicker, PendingAction, PendingManager,
    PickerAction, SearchRow, SessionPicker, SessionPickerAction, SessionSearch,
    SessionSearchAction, ThemePicker, ThemePickerAction, TurnPicker, TurnPickerAction,
    VariantPicker, VariantPickerAction, is_command_palette_shortcut, turn_entries,
};
use questions::QuestionAction;
use ratatui::style::Color;
use sidebar::{AgentRow, AgentTarget};
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

/// The root turn's stall watchdog, warning half. Children get 600s and
/// a summary cancellation (`ilar::subagent`); the root warns instead —
/// a persistent notice with the climbing silence and one bell — because
/// the person it would interrupt is sitting right here. The clock
/// measures literal nothing: no stream bytes, no tool input progress.
/// A tool call in flight holds it entirely (`decide::stall_verdict`),
/// and a tool finishing restarts it (`App::set_activity` re-seeds
/// `stream_last_data` on re-entering a streaming state).
const ROOT_STALL_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(300);
/// The abort half: after twice the warning threshold of literal
/// nothing, cancel through the same token Esc uses, so the turn ends as
/// an ordinary abort — transcript closed honestly, committed chain kept
/// for resume — never a silent disappearance.
const ROOT_STALL_ABORT_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

/// What a person at the terminal does about a store left locked: the
/// password is asked for once, before the screen is taken over, and
/// held for the life of the process. Every refusal the lock causes ends
/// with this.
const UNLOCK_HINT: &str = "restart ilar and type the master password at the start prompt";

/// A sealed secret store is unlocked once, on the plain terminal,
/// before anything else runs; an empty answer leaves it locked for the
/// session. The configuration is resolved again afterwards, since a
/// provider key may live in the store.
fn unlock_secrets(config: ilar::config::Config) -> Result<ilar::config::Config> {
    let store = ilar::secrets::SecretStore::open(config.state_dir());
    if !store.is_locked() {
        return Ok(config);
    }
    let unlocked = secret_cli::unlock_if_sealed(
        &store,
        secret_cli::STARTUP_PROMPT,
        secret_cli::STARTUP_TRIES,
        &mut secret_cli::ask_on_terminal,
    );
    match unlocked {
        Ok(true) => Loader::new().resolve().context("loading config"),
        // Enter, or the last of three typos: a locked store is a
        // session with no stored secrets, not a reason to refuse to run.
        Ok(false) => {
            eprintln!(
                "The secret store stays locked this session: no stored secret can be used. \
                 Restart ilar to type the master password again."
            );
            Ok(config)
        }
        // No terminal to ask on — cron, systemd, a pipe — is the same
        // session, said once and carried on with.
        Err(error) => {
            eprintln!("The secret store stays locked this session: {error:#}.");
            Ok(config)
        }
    }
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Log in to OpenAI with your ChatGPT account (OAuth in the browser)
    Login,
    /// Store, list and grant the secrets tools may ask for
    Secret {
        #[command(subcommand)]
        command: secret_cli::SecretCommand,
    },
    /// Run one turn without a terminal and print the answer
    Exec(ExecArgs),
    /// Read the session store over HTTP (read-only, no turns run)
    #[cfg(feature = "serve")]
    Serve(ServeArgs),
}

#[cfg(feature = "serve")]
#[derive(clap::Args, Debug)]
struct ServeArgs {
    /// Address to bind (default 127.0.0.1:4527, falling back to an
    /// ephemeral port when taken); anything but loopback requires a
    /// token, and an explicit address never falls back
    #[arg(long)]
    bind: Option<std::net::SocketAddr>,

    /// Open the page in a browser once the server is up
    #[arg(long)]
    open: bool,

    /// Session tail poll interval in milliseconds (default 250);
    /// overrides ILAR_SERVE_POLL_MS
    #[arg(long)]
    poll_ms: Option<u64>,
}

#[derive(clap::Args, Debug)]
struct ExecArgs {
    /// The prompt; omit it to read it from stdin
    prompt: Option<String>,

    /// Model to use (provider/model-id); overrides config
    #[arg(long)]
    model: Option<String>,

    /// Agent name from config (markdown agents)
    #[arg(long)]
    agent: Option<String>,

    /// Session id to resume
    #[arg(long)]
    session: Option<String>,

    /// Resume the most recently modified session
    #[arg(long = "continue", conflicts_with = "session")]
    continue_last: bool,

    /// Emit the loop's events as NDJSON on stdout instead of the answer
    #[arg(long)]
    json: bool,

    /// Ignore the working directory's AGENTS.md/CLAUDE.md
    #[arg(long)]
    no_project_instructions: bool,

    /// Use them even when general.project_instructions is off
    #[arg(long, conflicts_with = "no_project_instructions")]
    project_instructions: bool,
}

/// What a fresh box needs and `--help` never said: where configuration
/// and state live, and which variable carries which provider's key.
/// Printed under the flags so the first `ilar --help` names a next step.
const AFTER_HELP: &str = "\
Configuration:
  ILAR_CONFIG_DIR   config directory (default ~/.config/ilar); ilar.toml lives here
  ILAR_STATE_DIR    sessions, secret store, auth tokens (default ~/.local/state/ilar)

Provider keys (or providers.<name>.api_key in ilar.toml, or `ilar secret set`):
  ILAR_ZAI_API_KEY        zai/<model>, the default general.model
  ILAR_OPENAI_API_KEY     openai/<model>; or `ilar login` for a ChatGPT account
  ILAR_OPENCODE_API_KEY   opencode/<model> and opencode-go/<model>

See docs/configuration.md; `ilar --print-prompt` shows what the model gets.";

#[derive(Parser, Debug)]
#[command(
    name = "ilar",
    version,
    about = "Personal coding agent",
    after_help = AFTER_HELP
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Model to use (provider/model-id); overrides config
    #[arg(long)]
    model: Option<String>,

    /// Session id to resume
    #[arg(long)]
    session: Option<String>,

    /// Open a session read-only: its transcript, followed live, with
    /// no writer lease taken — the way to look at a gateway chat.
    /// Nothing about the session is decided here, so the flags that
    /// would decide one are refused rather than ignored
    #[arg(long, conflicts_with_all = ["session", "continue_last", "model", "agent", "print_prompt"])]
    view: Option<String>,

    /// Resume the most recently modified session
    #[arg(long = "continue", conflicts_with = "session")]
    continue_last: bool,

    /// Agent name from config (markdown agents)
    #[arg(long)]
    agent: Option<String>,

    /// Print what the model would get — model, options, system prompt,
    /// every tool with its schema — and exit
    #[arg(long)]
    print_prompt: bool,

    /// Ignore the working directory's AGENTS.md/CLAUDE.md
    #[arg(long)]
    no_project_instructions: bool,

    /// Use them even when general.project_instructions is off
    #[arg(long, conflicts_with = "no_project_instructions")]
    project_instructions: bool,
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
        if self.terminal_initialized {
            // The window is ours only while we run. Left as "ilar —
            // <topic>" it names a session that ended, in a shell that
            // has moved on; an empty title hands the name back, and
            // the shell's own prompt hook takes it from there.
            let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(""));
        }
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
    if let Some((name, args)) = parse_slash_invocation(&text)
        && decide::MAINTENANCE_COMMANDS.contains(&name)
    {
        if !args.is_empty() {
            app.input = InputBuffer::from(text.as_str());
            app.set_notice(decide::maintenance_usage(name), NoticeLevel::Warning);
            return None;
        }
        match name {
            "compact" => {
                app.compact_requested = true;
                app.set_notice("compaction starting", NoticeLevel::Info);
            }
            "rewind" => app.turn_picker_requested = true,
            "sessions" => app.session_search = Some(modals::SessionSearch::new()),
            "fork" => app.fork_requested = true,
            // Structurally unreachable — the guard above matched this
            // name against the same list — but a panic here would take
            // the whole TUI down over a typo in one of the two.
            other => {
                debug_assert!(false, "unhandled maintenance command /{other}");
                app.input = InputBuffer::from(text.as_str());
                app.set_notice(format!("/{other} is not wired up"), NoticeLevel::Error);
            }
        }
        return None;
    }
    if let Some(("btw", question)) = parse_slash_invocation(&text) {
        if question.trim().is_empty() {
            app.input = InputBuffer::from(text.as_str());
            app.set_notice(decide::ASIDE_USAGE, NoticeLevel::Warning);
        } else {
            app.aside_requested = Some(question.to_string());
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
            let notice = app.abort_goal().unwrap_or_else(|| "no active goal".into());
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

#[derive(Debug, Clone, PartialEq)]
enum TurnRequest {
    New(String, Vec<ilar::session::ImageContent>),
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
        Intent::OpenContextPicker => {
            app.open_context_picker();
            None
        }
        Intent::SetContextWindow(choice) => {
            app.set_context_override(choice);
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
            // Whatever is attached rides this message, exactly as it
            // rides a fresh turn: steering is when a screenshot is most
            // useful, and holding the images back would deliver words
            // the model cannot act on.
            let message = ilar::agent::Steer {
                text,
                images: std::mem::take(&mut app.pending_images),
            };
            // The channel can close between the decision and here — the
            // turn ending is exactly when that happens — and the message
            // must not be lost with it.
            match steer {
                Some(tx) if tx.send(message.clone()).is_ok() => {
                    // No notice: the pending strip above the input now
                    // shows the message itself, with its fate.
                    app.pending_steers.push(message);
                    None
                }
                // Queued directly rather than through `Intent::Queue`:
                // the images are already off the prompt and travel with
                // the message they were attached to.
                _ => {
                    app.queued_messages.push(message);
                    None
                }
            }
        }
        Intent::Queue(text) => {
            app.queued_messages.push(ilar::agent::Steer {
                text,
                images: std::mem::take(&mut app.pending_images),
            });
            None
        }
        Intent::Aside(question) => {
            // Consumed by settle, which hands it to the runtime's
            // detached aside task.
            app.aside_requested = Some(question);
            None
        }
        Intent::PastePalette(text) => {
            if let Some(palette) = app.command_palette.as_mut() {
                palette.insert_query(&text);
            }
            None
        }
        Intent::PasteSearch(text) => {
            // Same single-line policy as the modal queries: control
            // characters dropped, capped, and a no-growth paste is inert.
            if modals::insert_query(&mut app.search_query, text.trim()) {
                app.search_refresh();
            }
            None
        }
        Intent::PasteQuestion(text) => {
            if let Some(question) = app.question_modal.as_mut() {
                question.paste(&text);
            }
            None
        }
        Intent::PastePassword(text) => {
            if let Some(modal) = app.password_modal.as_mut() {
                modal.paste(&text);
            }
            None
        }
        Intent::PasteModalQuery(text) => {
            // Whichever picker owns the keyboard filters on it, exactly
            // as if the text had been typed. The modal decided this, so
            // only the ones with a query can arrive here.
            match app.active_modal() {
                Some(Modal::SessionSearch) => {
                    if let Some(search) = app.session_search.as_mut() {
                        // The Rescan is acted on by the loop, which owns
                        // the scan handles; the modal already marked
                        // itself as wanting one.
                        search.insert_query(&text);
                    }
                }
                Some(Modal::SessionPicker) => {
                    if let Some(picker) = app.session_picker.as_mut() {
                        picker.insert_query(&text);
                    }
                }
                Some(Modal::TurnPicker) => {
                    if let Some(picker) = app.turn_picker.as_mut() {
                        picker.insert_query(&text);
                    }
                }
                Some(Modal::LinkPicker) => {
                    if let Some(picker) = app.link_picker.as_mut() {
                        picker.insert_query(&text);
                    }
                }
                Some(Modal::ModelPicker) => {
                    if let Some(picker) = app.model_picker.as_mut() {
                        picker.insert_query(&text);
                    }
                }
                Some(Modal::ThemePicker) => {
                    // A filter paste can only preview; choosing and
                    // dismissing stay on the key path, so there is
                    // nothing to persist here.
                    if let Some(ThemePickerAction::Preview(preview)) = app
                        .theme_picker
                        .as_mut()
                        .map(|picker| picker.insert_query(&text))
                    {
                        app.theme = preview;
                    }
                }
                other => debug_assert!(false, "{other:?} was routed a query paste it cannot hold"),
            }
            None
        }
        Intent::PasteInput(text) => {
            app.model_key_pending = false;
            app.clear_transient_notice();
            // A terminal drop arrives as pasted file paths; when every
            // token is an existing image file, attaching is the intent.
            // Not in a focus view: a message to an agent carries text
            // only, so an attachment there would be promised and then
            // silently stashed.
            if crate::app::dropped_image_paths(&text).is_some() && app.focus.is_some() {
                app.set_notice(
                    "images belong to the session behind this view — Esc leaves the view first",
                    NoticeLevel::Warning,
                );
            } else if let Some(paths) = crate::app::dropped_image_paths(&text)
                && paths.iter().all(|path| path.is_file())
            {
                let total = paths.len();
                let attached = paths
                    .iter()
                    .filter(|path| app.attach_image_file(path))
                    .count();
                // One summary for a multi-file drop; a total refusal
                // keeps the per-image reason already on screen.
                if total > 1 && attached == total {
                    app.set_notice(
                        format!(
                            "{total} images attached — send with your next message, Esc discards"
                        ),
                        NoticeLevel::Info,
                    );
                } else if total > 1 && attached > 0 {
                    app.set_notice(
                        format!("attached {attached} of {total} images — the rest were refused"),
                        NoticeLevel::Warning,
                    );
                }
            } else {
                app.input.insert(&text);
            }
            None
        }
        Intent::SendQueued => {
            let next = (!app.queued_messages.is_empty()).then(|| app.queued_messages.remove(0))?;
            // Back onto the prompt's attachments, ahead of anything
            // attached since: `StartTurn` takes whatever is pending,
            // which is the one path images reach a turn by, and a
            // queued message must not be a second one.
            app.pending_images.splice(0..0, next.images);
            apply_intent(app, Intent::StartTurn(next.text), steer)
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
            // Whatever is attached rides this turn; the transcript row
            // shows a marker per image in place of the payload.
            let images = std::mem::take(&mut app.pending_images);
            app.retry_available = false;
            app.turn_committed = false;
            app.clear_notice();
            // A completion steered into a dying turn comes back through
            // here, and it is a notification envelope, not something the
            // user typed: the same fold the steer path uses, or the live
            // transcript shows raw XML where the replay shows a
            // collapsed row.
            app.push_user_message(&text, &images);
            app.follow_tail = true;
            app.busy = true;
            app.status = "thinking".into();
            app.set_activity(Activity::Thinking);
            Some(TurnRequest::New(text, images))
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
    (
        "sessions",
        "grep every session's content and switch (^G: classic list)",
    ),
    (
        "btw",
        "ask a quick aside about the session; nothing is recorded",
    ),
    (
        "context",
        "override the context window this session assumes (32k…1M, default)",
    ),
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

This is a goal-mode session: after each of your turns you will be \
         asked to verify progress with concrete evidence. If no automatic \
         verification exists yet (tests, a replay harness, a checker \
         script), building one is part of the goal. Do not claim success \
         without evidence."
    )
}

fn goal_continuation_prompt(goal: &str, round: u32) -> String {
    format!(
        "Goal check, round {round}/{MAX_GOAL_ROUNDS}. The goal: {goal}

Verify the current state with concrete evidence by running your \
         verification (tests, harness, checker) now — do not judge from \
         memory. If the goal is genuinely achieved, output a line starting \
         with `{GOAL_SENTINEL}:` followed by the evidence. Otherwise state \
         what is still missing and continue working toward the goal in this \
         same turn."
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
        retry_available: app.retry_available,
        model_key_pending: app.model_key_pending,
    }
}

/// Hand a closed prompt's answer to the tool waiting on it, and note
/// it in the transcript. The asker can go away between the loop's
/// closed-check and the keypress that answers, so the transcript must
/// not claim an answer nobody received.
fn deliver_answer<T>(
    app: &mut App,
    reply: &mut Option<tokio::sync::oneshot::Sender<T>>,
    answer: T,
    line: String,
    status: &str,
) {
    let delivered = reply.take().is_some_and(|reply| reply.send(answer).is_ok());
    if delivered {
        app.push_transcript_line(Line_::System(line));
        app.status = status.into();
        app.set_activity(Activity::Tools);
    } else {
        app.push_transcript_line(Line_::System(format!(
            "{line} — but the tool had stopped waiting"
        )));
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
    SlashResolution::Unknown(close_skill_matches(&app.slash_inventory(), name))
}

/// Why a session cannot be resumed directly, if it cannot. Both the
/// picker and the content search validate before switching, so a bad
/// entry degrades to a notice here instead of failing the app's restart
/// after the modal is gone.
fn direct_resume_blocked(store: &SessionStore, id: &str) -> Option<String> {
    // The head is enough — the gate reads metadata — and it is one
    // file open instead of a whole replay on every picker action.
    match store
        .head(id)
        .map(|head| ensure_direct_resume_allowed(Some(&head.meta)))
    {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!(
            "cannot resume {}: {error}",
            session_name(store, id)
        )),
        Err(error) => Some(format!(
            "cannot resume {}: {error}",
            session_name(store, id)
        )),
    }
}

/// Abandon the content-search walk in flight: the flag stops the worker
/// and dropping the receiver stops draining rows it already queued. A
/// modal that wants a fresh scan just leaves `scanning` set — the
/// spawner above the dispatch starts the next one.
fn stop_session_scan(
    cancel: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    rx: &mut Option<(u64, std::sync::mpsc::Receiver<Vec<SearchRow>>)>,
) {
    if let Some(flag) = cancel.take() {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    *rx = None;
}

/// Everything a session switch must let go of: the background spawner,
/// and the turns that run beside the conversation — the aside and the
/// topic naming. All three are about the session being left, all three
/// keep a provider stream open, and none has anywhere to land once the
/// switch happens: their answers would arrive into a transcript that is
/// no longer theirs. One call, so the ritual cannot be half-performed
/// at the seventh exit.
async fn leave_session(
    spawner: &ilar::subagent::SubagentSpawner,
    aside_cancel: &mut Option<CancellationToken>,
    aside_handle: &mut Option<AsideHandle>,
    topic_handle: &mut Option<tokio::task::JoinHandle<Option<String>>>,
) {
    spawner.shutdown().await;
    if let Some(token) = aside_cancel.take() {
        token.cancel();
    }
    if let Some(handle) = aside_handle.take() {
        // Cancelled first, so this is the clean stop; aborted too, so a
        // provider that will not close cannot hold the switch. Aborts
        // land on await points, and the session appends between them are
        // synchronous.
        handle.abort();
        let _ = handle.await;
    }
    // Titling has no token — it is one short request — so the abort is
    // the whole stop.
    if let Some(handle) = topic_handle.take() {
        handle.abort();
        let _ = handle.await;
    }
}

fn close_skill_matches(inventory: &[(String, String)], name: &str) -> Vec<String> {
    let lowered = name.to_lowercase();
    let mut matches: Vec<String> = inventory
        .iter()
        .filter(|(candidate, _)| candidate.to_lowercase().contains(&lowered))
        .map(|(candidate, _)| candidate.clone())
        .collect();
    if matches.is_empty() {
        matches = inventory
            .iter()
            .map(|(candidate, _)| candidate.clone())
            .collect();
        // Nothing looked like it, so this is a bare listing — and the
        // inventory leads with the built-ins, which would fill the
        // whole list and hide every command and skill the user has. They
        // are in the help; what the typo was aiming at probably is not.
        matches.sort_by_key(|candidate| {
            BUILTIN_SLASH_COMMANDS
                .iter()
                .any(|(builtin, _)| candidate == builtin)
        });
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
    // One replay serves both the append and the measurement.
    let session = persist_model_change(resolver, store, session_id, &model, variant.as_deref())?;
    app.current_model = model.clone();
    app.current_variant = variant.clone();
    app.set_model_context_limit(display_context_limit(resolver, &model));
    app.context_used = ilar::compaction::estimate_tokens_with_request(
        &session,
        Some(system_prompt),
        &registry.definitions(),
    );
    app.context_estimated = true;
    drop(session);
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
    let cwd = std::env::current_dir().context("no cwd")?;
    // The same rule the TUI follows — this directory's latest — since
    // the docs promise `--continue` behaves the same in both drivers,
    // and a script that resumes another checkout's conversation
    // against these files is the worse surprise of the two.
    let mut continued_elsewhere = None;
    let resume = if args.continue_last {
        let (id, elsewhere) = ilar::runtime::latest_session_in(&store, &cwd)?;
        continued_elsewhere = elsewhere;
        Some(id)
    } else {
        args.session
    };
    let mut plan = ilar::runtime::RuntimePlan::resolve(
        config,
        &ilar::runtime::RuntimeOptions {
            model: args.model,
            agent: args.agent,
            resume,
            cwd: cwd.clone(),
            // Nobody is here to answer: the tool is left off so the
            // model is told so on the spot instead of blocking.
            questions: false,
            grants: false,
            project_instructions: cli_project_instructions(
                args.project_instructions,
                args.no_project_instructions,
            ),
            context_files: None,
            user_dir: None,
            own_skills_only: false,
            unlock_hint: Some(UNLOCK_HINT.to_string()),
        },
    )?;
    let mut plan_notices = std::mem::take(&mut plan.notices);
    if let Some(notice) = continued_elsewhere {
        // First: it says which conversation everything below is about.
        plan_notices.insert(0, notice);
    }
    let notices = startup_notices(
        config.warnings.clone(),
        plan_notices,
        plan.skipped_project_instructions,
        args.no_project_instructions,
    );
    let runtime = plan.start(config)?;

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
    // Said before the turn, and said at all: a project `ilar.toml` with
    // a `[providers]` table was ignored here in complete silence, which
    // reads as the setting not existing.
    exec::emit_notices(&notices, format, &mut out, &mut err)?;
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
    // A turn that never reached the provider — a bad key, a refused
    // model — leaves a session with nothing in it. It goes with the
    // run that made it.
    ilar::runtime::end_session(config, &runtime.store, &runtime.session_id);

    if let Err(error) = &outcome {
        let _ = writeln!(err, "error: {error:#}");
    }
    Ok(exec::exit_code(&outcome))
}

/// `None` when neither flag was given, leaving the decision to
/// configuration. Clap rules the pair out together; if that ever
/// changes, refusing the file is the safe reading.
fn cli_project_instructions(include: bool, skip: bool) -> Option<bool> {
    match (include, skip) {
        (_, true) => Some(false),
        (true, false) => Some(true),
        (false, false) => None,
    }
}

/// The line a session opens with when the project put instructions in
/// the working directory and this launch did not use them — named as
/// written, and blaming whichever of the two knobs actually did it.
/// Telling someone a flag they never typed dropped their file is worse
/// than saying nothing.
fn project_instructions_notice(skipped: Option<&str>, by_flag: bool) -> Option<String> {
    let file = skipped?;
    let cause = if by_flag {
        "--no-project-instructions"
    } else {
        "general.project_instructions = false"
    };
    Some(format!("project {file} present but skipped ({cause})"))
}

/// The system lines a session opens with: settings that parsed but were
/// not honoured, what this launch asked for and did not get, then the
/// project file that exists but was left out. All three say out loud
/// that something the user wrote was not used — silence there reads as
/// a bug in the program.
fn startup_notices(
    config_warnings: Vec<String>,
    launch_notices: Vec<String>,
    skipped: Option<&str>,
    by_flag: bool,
) -> Vec<String> {
    let mut lines = config_warnings;
    lines.extend(launch_notices);
    lines.extend(project_instructions_notice(skipped, by_flag));
    lines
}

/// What to put in `ilar.toml` for the tokens `ilar login` just stored
/// to be used. Without it the login succeeds and the next `ilar` runs
/// the default `zai/glm-4.7` and dies naming a key the person never
/// had: the account is stored, nothing points at it.
fn chatgpt_setup_hint(config_path: &std::path::Path) -> String {
    format!(
        "\nNothing points at that account yet. Add to {}:\n\n  \
         [providers.openai]\n  auth = \"chatgpt\"\n\n  \
         [general]\n  model = \"{}\"\n\n\
         Any openai/… model works; `m` in the TUI lists the ones this account can reach.\n",
        config_path.display(),
        ilar::model::CHATGPT_SUGGESTED_MODEL,
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // The directories come from the environment alone, before any file
    // is read. A subcommand that touches nothing but the state
    // directory stops here: a broken `ilar.toml` — or an endpoint that
    // takes three seconds to refuse a listing — must not stand between
    // someone and the credential they came to fix.
    let dirs = Loader::new().resolve_dirs();
    dirs.require_home()?;
    if let Some(Command::Secret { command }) = args.command {
        let store = ilar::secrets::SecretStore::open(&dirs.state);
        // A piped value is read as it is; at a terminal it is asked
        // for hidden and confirmed, so nothing is ever echoed.
        use std::io::IsTerminal;
        let mut stdin = std::io::stdin().lock();
        let piped: Option<&mut dyn std::io::Read> = if stdin.is_terminal() {
            None
        } else {
            Some(&mut stdin)
        };
        let text = secret_cli::run(&store, command, piped, &mut secret_cli::ask_on_terminal)?;
        println!("{text}");
        return Ok(());
    }
    if let Some(Command::Login) = args.command {
        let store = ilar::auth::AuthStore::open(dirs.state.clone());
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
        print!("{}", chatgpt_setup_hint(&dirs.config.join("ilar.toml")));
        return Ok(());
    }
    let config = Loader::new().resolve().context("loading config")?;
    if let Some(Command::Exec(exec_args)) = args.command {
        let config = unlock_secrets(config)?;
        let code = run_exec(&config, exec_args).await?;
        std::process::exit(code);
    }
    // Serving reads the store, and drives the sessions it can take the
    // writer lease on. Nothing about a read consults the configuration
    // beyond the state directory, so a machine with no provider
    // configured still browses what it already recorded — the write path
    // resolves its runtime per turn and fails there if it must.
    #[cfg(feature = "serve")]
    if let Some(Command::Serve(serve_args)) = args.command {
        return serve::run(
            &config,
            serve::ServeOptions {
                bind: serve_args.bind,
                open: serve_args.open,
                poll_ms: serve_args.poll_ms,
            },
        )
        .await;
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
    let mut carried: Option<(
        Option<String>,
        Option<String>,
        Vec<crate::app::StashedPrompt>,
    )> = None;
    let mut first_run = true;
    let mut terminal_hold: Option<(ratatui::DefaultTerminal, TerminalSession)> = None;
    if let Some(id) = args.view.as_deref() {
        return watch::run(&config, id, configured_theme).await;
    }
    // Before the terminal is taken over: the master password is typed
    // on the plain terminal, once, and the configuration is read again
    // so a provider key kept in the store is seen.
    let config = unlock_secrets(config)?;
    let mut active_theme = configured_theme;
    // Settings that parsed but were not honoured. Shown once, on the
    // first session: they are a property of the config, not the session.
    let mut config_warnings = config.warnings.clone();

    loop {
        let resume_target = if first_run {
            if args.continue_last {
                // This directory's newest session first: the newest one
                // overall may belong to another checkout, and opening it
                // here would point its conversation at these files. The
                // fallback still opens, and says where it started.
                let here = std::env::current_dir().context("no cwd")?;
                let (id, elsewhere) = ilar::runtime::latest_session_in(&store, &here)?;
                if let Some(notice) = elsewhere {
                    // Carried rather than notified: the app does not
                    // exist yet, and `carried` is how a notice reaches
                    // the session being opened.
                    carried = Some((None, Some(notice), Vec::new()));
                }
                Some(id)
            } else {
                args.session.clone()
            }
        } else {
            session_override.clone()
        };
        // What a bare launch offers: this directory's last session,
        // ghosted, for one key. Only on a launch that named no session
        // — `--session`, `--continue` and `--view` have all said which
        // conversation this is — and read here, before the fresh
        // session is created, because creating one moves the pointer
        // the offer comes from.
        let offer = if first_run
            && resume_target.is_none()
            && config.general.resume_offer
            // `--print-prompt` prints and exits; there is no screen to
            // offer anything on.
            && !args.print_prompt
        {
            let here = std::env::current_dir().context("no cwd")?;
            session_view::ghost_offer(&store, &here, std::time::SystemTime::now())
        } else {
            None
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
        let mut plan = ilar::runtime::RuntimePlan::resolve(
            &config,
            &ilar::runtime::RuntimeOptions {
                model: cli_model.map(str::to_string),
                agent: cli_agent.map(str::to_string),
                resume: resume_target.clone(),
                cwd: cwd.clone(),
                questions: true,
                grants: true,
                // Not first-run-only like the model and agent
                // overrides: what this launch does with the project
                // file is a property of the launch, so every session
                // this process opens — resumed, switched or forked —
                // honours it.
                project_instructions: cli_project_instructions(
                    args.project_instructions,
                    args.no_project_instructions,
                ),
                context_files: None,
                user_dir: None,
                own_skills_only: false,
                unlock_hint: Some(UNLOCK_HINT.to_string()),
            },
        )?;
        if args.print_prompt {
            println!("{}", plan.preview(&config)?.render());
            return Ok(());
        }
        let skipped_project_instructions = plan.skipped_project_instructions;
        let launch_notices = std::mem::take(&mut plan.notices);
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
            grants,
            resumed,
            ..
        } = runtime;
        let question_rx = questions.expect("the TUI asked for questions");
        let ask_rx = grants.expect("the TUI asked for grants");
        let notifications = spawner.subscribe();
        let subagent_activity = spawner.subscribe_activity();
        let model_choices = config.available_models();

        let user_config_path = config.dirs().0.join("ilar.toml");

        let context_limit = display_context_limit(resolver.as_ref(), &model_for_session);
        let mut app = App::new();
        app.cache_compact = config.cache_compact.clone();
        app.theme = active_theme;
        app.history = history::PromptHistory::load(config.state_dir().join("prompt_history.jsonl"));
        app.skills = skill_inventory;
        app.commands = command_inventory;
        app.available_models = model_choices.iter().map(|model| model.full_id()).collect();
        app.session_id = session_id.clone();
        app.todos = todos;
        // Said once on stderr before the screen came up; from here on it
        // is the notice row's job to remember it.
        app.secrets_locked = ilar::secrets::SecretStore::open(config.state_dir()).is_locked();
        // The reader in hand answers the pending-question check;
        // run_app takes the answer instead of re-reading the log for
        // it. A fresh session cannot have one.
        let mut initial_pending_question_id = None;
        // A resumed session's restore — the whole-log fold plus a
        // store load per child, recursively — runs on a blocking
        // worker so the terminal comes up drawing instead of frozen
        // for the length of the replay. The reader's cheap answers
        // (topic, pending question) are taken here; the reader itself
        // moves to the worker, which also prices the context while it
        // holds the events. A prompt typed while it runs queues, and
        // the landing sends the queue. Fresh sessions price inline:
        // the request is all there is.
        let restore_handle = if let Some(resumed) = resumed {
            app.topic = resumed.topic().map(str::to_string);
            if let Some(pending) = resumed.pending_question() {
                initial_pending_question_id = Some(pending.tool_call_id.clone());
                app.question_modal = Some(questions::QuestionModal::new(pending.request.clone()));
            }
            // Where the history belongs: after what the app already
            // holds (the banner), ahead of everything pushed while
            // the worker runs.
            let restore_at = app.lines().len();
            let store = store.clone();
            let system_prompt = system_prompt.clone();
            let definitions = registry.definitions();
            Some((
                restore_at,
                tokio::task::spawn_blocking(move || {
                    // Settled: a session is switched into when nothing is
                    // driving it, so whatever it left running died with
                    // the process that ran it.
                    let view = session_view::restored_session_view_with_store(
                        &resumed,
                        &store,
                        session_view::Liveness::Settled,
                    );
                    let priced = ilar::compaction::estimate_reader_tokens_with_request(
                        &resumed,
                        Some(&system_prompt),
                        &definitions,
                    );
                    (view, priced)
                }),
            ))
        } else {
            None
        };
        let (context_used, context_estimated) = if restore_handle.is_some() {
            // Zero until the landing prices it — a muted "0%", not a
            // wrong exact number.
            (0, true)
        } else {
            session_context_tokens(&store, &session_id, &system_prompt, &registry)?
        };
        app.configure_runtime(
            model_for_session.clone(),
            reasoning_for_session,
            cwd.clone(),
            context_used,
            context_limit,
            context_estimated,
        );
        if restore_handle.is_some() {
            app.busy = true;
            app.status = "restoring session".into();
            app.set_activity(Activity::Thinking);
        }
        if app.question_modal.is_some() {
            // The question outranks the restore: the user can answer
            // while history loads underneath the modal.
            app.busy = true;
            app.status = "waiting for your answer".into();
            app.set_activity(Activity::Paused);
        }
        // The config warnings are drained: they are a property of the
        // configuration and are said once. The skipped project file is
        // a property of every prompt this launch assembles, so a
        // session entered later through the picker says it again.
        for line in startup_notices(
            std::mem::take(&mut config_warnings),
            launch_notices,
            skipped_project_instructions,
            args.no_project_instructions,
        ) {
            app.push_transcript_line(Line_::System(line));
        }
        if let Some((prefill, notice, stash)) = carried.take() {
            if let Some(prefill) = prefill {
                app.input = InputBuffer::from(prefill.as_str());
            }
            if let Some(notice) = notice {
                app.set_notice(notice, NoticeLevel::Info);
            }
            // Stashed prompts belong to the person, not to the session
            // they were put aside in.
            app.input_stash = stash;
        }
        // Last, so nothing above resets the status line under it: the
        // offer says what is on screen until a choice is made.
        if let Some(ghost) = offer {
            app.offer_session(ghost);
        }

        if terminal_hold.is_none() {
            terminal_hold = Some(TerminalSession::start()?);
        }
        app.keyboard_enhanced = terminal_hold
            .as_ref()
            .is_some_and(|(_, session)| session.keyboard_enhanced);
        // Completions published by an earlier run of this tree that
        // never reached their session: the outbox kept them, and they
        // enter this run as if freshly notified — held for the user.
        // The scan loads a parent log per outbox file and walks
        // ancestry, so it joins the restore off the UI task; its
        // results land in the loop.
        let outbox_dir = ilar::runtime::outbox_dir(&config);
        let adoption_handle = {
            let store = store.clone();
            let outbox_dir = outbox_dir.clone();
            let session_id = session_id.clone();
            tokio::task::spawn_blocking(move || {
                ilar::outbox::pending(&store, &outbox_dir, &session_id)
            })
        };
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
            adoption_handle,
            notifications,
            subagent_activity,
            question_rx,
            ask_rx,
            loop_config,
            model_choices,
            services,
            outbox_dir,
            initial_pending_question_id,
            restore_handle,
        )
        .await?;
        active_theme = app.theme;
        // The session this run is leaving — quit or switch. One created
        // by the launch and never typed into leaves nothing behind;
        // before this, every `ilar` opened and closed left a
        // "(no messages yet)" row for ever.
        ilar::runtime::end_session(&config, &store, &session_id);
        match exit {
            AppExit::Quit => return Ok(()),
            AppExit::SwitchInto {
                id,
                prefill,
                notice,
                stash,
            } => {
                session_override = Some(id);
                carried = Some((prefill, notice, stash));
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

/// The first uuid segment: enough to tell sessions apart in a notice,
/// short enough to leave room for what actually matters.
pub(crate) fn short_session_id(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// What a session with no prompt yet is called in every listing — the
/// same words the picker uses, never its id.
const UNTITLED_SESSION: &str = "(no messages yet)";

/// Characters of a session's opening prompt a label carries.
const LABEL_TITLE_CHARS: usize = 48;
/// Columns a roster row's "for …" note may take.
const FOREIGN_PARENT_CHARS: usize = 24;

/// A session by name rather than by id, for every message that says
/// where a task result went: this session; a running or delivering
/// child by its roster row's agent and task; anything else by the
/// agent and opening prompt its log head records; and only a session
/// with no readable head by its short id. The head read is a file open,
/// so it happens once per id and is remembered.
fn session_label(
    app: &App,
    store: &SessionStore,
    own_session_id: &str,
    cache: &mut std::collections::HashMap<String, String>,
    session_id: &str,
) -> String {
    if session_id == own_session_id {
        return "this session".into();
    }
    if let Some(row) = app
        .agents_view
        .iter()
        .find(|row| row.session_id == session_id)
    {
        return format!("{} · {}", row.agent, row.description);
    }
    if let Some(label) = cache.get(session_id) {
        return label.clone();
    }
    let label = session_name(store, session_id);
    cache.insert(session_id.to_string(), label.clone());
    label
}

/// A session named from its log head alone — agent and opening prompt
/// — for the places that have no roster and no cache: a picker action,
/// a resume refusal, a fork notice. `session <id>` only when the head
/// cannot be read, which for a session being deleted is the moment
/// after; read the name first.
fn session_name(store: &SessionStore, session_id: &str) -> String {
    match store.head(session_id) {
        Ok(head) => {
            let title = head.title.map(|title| {
                let mut chars = title.chars();
                let short: String = chars.by_ref().take(LABEL_TITLE_CHARS).collect();
                if chars.next().is_some() {
                    format!("{short}…")
                } else {
                    short
                }
            });
            match title {
                Some(title) => format!("{} · {title}", head.meta.agent),
                None => format!("{} · {}", head.meta.agent, short_session_id(session_id)),
            }
        }
        Err(_) => format!("session {}", short_session_id(session_id)),
    }
}

/// Outbox-recovered completions enter as held parcels — and their
/// presence asks for a delivery pause until the user's first
/// completed turn. Opening a session is reading, not summoning: a
/// backlog from a previous run must not spend tokens, mutate logs,
/// or run tools before the user has said anything. The caller waives
/// the pause when the user has already engaged by the time the scan
/// lands. An empty recovery asks for nothing.
fn adopt_recovered(
    recovered: Vec<ilar::subagent::Notification>,
) -> (std::collections::VecDeque<ilar::delivery::Parcel>, bool) {
    let held: std::collections::VecDeque<ilar::delivery::Parcel> = recovered
        .into_iter()
        .map(ilar::delivery::Parcel::fresh)
        .collect();
    let paused = !held.is_empty();
    (held, paused)
}

/// Held-back notifications first — they arrived earlier and were only
/// deferred — then whatever the channel has.
fn next_notification(
    held: &mut std::collections::VecDeque<ilar::delivery::Parcel>,
    notifications: &mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
) -> Option<ilar::delivery::Parcel> {
    held.pop_front().or_else(|| {
        notifications
            .try_recv()
            .ok()
            .map(ilar::delivery::Parcel::fresh)
    })
}

/// Move everything the channel is holding into the backlog, in arrival
/// order. The backlog is the queue every surface reads — the notice
/// row, the pending manager, the quit cost — so a completion sitting
/// unread in the channel is a completion nobody can see.
fn drain_into_backlog(
    held: &mut std::collections::VecDeque<ilar::delivery::Parcel>,
    notifications: &mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
) {
    while let Ok(queued) = notifications.try_recv() {
        held.push_back(ilar::delivery::Parcel::fresh(queued));
    }
}

/// A propagated completion goes behind what was already queued when it
/// landed: the channel is drained into the held queue first, then the
/// propagated one takes the back seat. A bare push_back would let it
/// jump the backlog, since held notifications are offered first.
fn hold_propagate_behind_backlog(
    held: &mut std::collections::VecDeque<ilar::delivery::Parcel>,
    notifications: &mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    parcel: ilar::delivery::Parcel,
) {
    drain_into_backlog(held, notifications);
    held.push_back(parcel);
}

/// A completion being delivered to another session, detached from the
/// turn slot the way an aside is: it resumes a child, so the root's
/// conversation owes it nothing. The notification rides along so a
/// failed delivery can still surface the child's final word.
struct RoutedDelivery {
    handle: tokio::task::JoinHandle<Result<ilar::subagent::RouteOutcome>>,
    cancel: CancellationToken,
    parcel: ilar::delivery::Parcel,
}

/// A message typed into a focus view, on its way to that agent through
/// the same path the model's `task_message` takes: a running agent is
/// steered, a finished one resumed with the message as its prompt. The
/// root keeps drawing; the ending lands as a transcript line.
struct FocusMessage {
    handle: tokio::task::JoinHandle<ilar::subagent::TaskMessage>,
    target: String,
}

/// The root's transcript line for a message sent from a focus view.
fn focus_message_line(target: &str, text: &str) -> String {
    format!("→ {target}: {text}")
}

/// How much of an agent's reply the headline carries.
const FOCUS_REPLY_CHARS: usize = 100;
/// How much of a failure it carries. Longer than a reply's: a reply is
/// on screen in the agent's own view, an error is only here.
const FOCUS_ERROR_CHARS: usize = 200;

/// The root's one line for what became of a focus message. The tool's
/// own wording is written for a model — "do not repeat the message",
/// with the task's uuid in it — and a resumed agent's whole answer, up
/// to 16 KiB of unrendered markdown, is not a record of anything: the
/// focus view already shows it rendered.
fn focus_outcome_line(target: &str, outcome: ilar::subagent::TaskMessage) -> (String, NoticeLevel) {
    use ilar::subagent::TaskMessage;
    match outcome {
        TaskMessage::Queued { .. } => (
            format!("{target} takes it at its next step"),
            NoticeLevel::Info,
        ),
        TaskMessage::Held { .. } => (
            format!(
                "{target} is busy with work of its own — it gets the message at its next resume"
            ),
            NoticeLevel::Info,
        ),
        TaskMessage::Refused(why) => (
            format!(
                "message to {target} was refused: {}",
                headline(&why, FOCUS_ERROR_CHARS)
            ),
            NoticeLevel::Error,
        ),
        // The resume declined but the message is still parked: a
        // failure to report, not a message to retype.
        TaskMessage::Answered {
            output,
            still_queued: true,
            ..
        } => (
            format!(
                "{target} could not be resumed ({}) — the message waits for its next resume",
                headline(&output.content, FOCUS_ERROR_CHARS)
            ),
            NoticeLevel::Warning,
        ),
        TaskMessage::Answered { output, .. } if output.is_error => (
            format!(
                "message to {target} failed: {}",
                headline(&output.content, FOCUS_ERROR_CHARS)
            ),
            NoticeLevel::Error,
        ),
        TaskMessage::Answered { output, .. } => (
            format!(
                "{target} answered: {}",
                headline(&output.content, FOCUS_REPLY_CHARS)
            ),
            NoticeLevel::Info,
        ),
    }
}

/// The first line worth showing, bounded. The task-id footer and the
/// "still queued" note the tool appends are not the answer.
fn headline(text: &str, chars: usize) -> String {
    let headline = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("(task_id:"))
        .unwrap_or("(nothing)");
    crate::text::truncate_display(headline, chars, crate::text::Truncation::Right)
}

enum TurnCompletion {
    /// The outcome, and any question the turn left unanswered in the
    /// log — read on the turn's own task, where the writer just kept
    /// the replay checkpoint warm, so the loop never pays a parse to
    /// learn it.
    Root(Result<TurnOutcome>, Option<ilar::session::PendingQuestion>),
    Compaction(
        Result<ilar::compaction::ManualCompactionOutcome>,
        Option<ilar::session::PendingQuestion>,
    ),
}

/// Whether the log holds a question awaiting an interactive answer.
/// Best-effort: a load failure reads as "none", and the crash path in
/// the loop still rechecks for itself.
fn stranded_question(
    store: &SessionStore,
    session_id: &str,
) -> Option<ilar::session::PendingQuestion> {
    store
        .load(session_id)
        .ok()
        .and_then(|reader| reader.pending_question().cloned())
}

/// Where a root turn picks up: the one thing the start sites disagree
/// on beyond the bell.
enum RootTurn {
    /// A prompt to send — the user's own, or a subagent notification
    /// delivered as one (nothing rides along with those).
    New(String, Vec<ilar::session::ImageContent>),
    /// Retry a turn that failed after committing its chain.
    Resume,
    /// The answer to a question the model paused on.
    Answer(ilar::question::QuestionResponse),
}

/// Whether the turn's completion arms the terminal bell. A turn the
/// loop started for itself — a subagent notification — is not one the
/// user is waiting on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bell {
    Ring,
    Silent,
}

/// The slots one root turn owns for its lifetime, borrowed out of the
/// loop's own locals.
struct TurnSlots<'a> {
    handle: &'a mut Option<tokio::task::JoinHandle<TurnCompletion>>,
    events_rx: &'a mut Option<LoopEventReceiver>,
    cancel: &'a mut Option<CancellationToken>,
    steer_tx: &'a mut Option<ilar::agent::SteerSender>,
    ring_on_completion: &'a mut bool,
}

/// What every root turn in the session is built from.
struct TurnDeps<'a> {
    resolver: &'a Arc<dyn ProviderResolver>,
    store: &'a SessionStore,
    session_id: &'a str,
    system_prompt: &'a str,
    registry: &'a ToolRegistry,
    tool_ctx: &'a ToolContext,
    loop_config: &'a LoopConfig,
}

/// A command's one-turn model override, validated by prepare_prompt;
/// the provider check happens in the adopt. On failure the turn still
/// runs — its prompt is already in the transcript — just under the
/// session's own model.
///
/// The override belongs to the turn the command started, so the answer
/// that resumes that turn across a question pause claims it too. A
/// retry-resume does not: it continues a turn that already adopted
/// whatever it was going to.
fn adopt_pending_model_override(app: &mut App, entry: &RootTurn, deps: &TurnDeps<'_>) {
    if matches!(entry, RootTurn::Resume) {
        return;
    }
    let Some((model, variant)) = app.pending_model_override.take() else {
        return;
    };
    let target = model.unwrap_or_else(|| app.current_model.clone());
    let revert = (app.current_model.clone(), app.current_variant.clone());
    match adopt_model_selection(
        app,
        deps.resolver.as_ref(),
        deps.store,
        deps.session_id,
        deps.system_prompt,
        deps.registry,
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

/// The one root-turn spawn ritual, shared by every site that starts
/// one: a loop-event channel the frame drains, a fresh cancellation
/// token, a steer channel for messages typed mid-turn, and the agent
/// entry point on a task whose `TurnCompletion::Root` the loop joins.
fn spawn_root_turn(
    app: &mut App,
    slots: TurnSlots<'_>,
    deps: TurnDeps<'_>,
    entry: RootTurn,
    bell: Bell,
) {
    // Every start site is guarded against starting a turn while one
    // runs. If that ever slips, the running turn would be orphaned —
    // its cancellation token dropped without firing, still writing to
    // the same session.
    debug_assert!(
        slots.handle.is_none(),
        "starting a turn while one is already running"
    );
    adopt_pending_model_override(app, &entry, &deps);
    // Seed the liveness clock at the spawn, not the first event: the
    // stall watchdog reads it on the very next pass, and an instant
    // left over from the previous turn must not count as this turn's
    // silence. `TurnStarted` re-seeds it moments later.
    app.stream_last_data = Some(std::time::Instant::now());
    let (tx, rx) = loop_event_channel(LOOP_EVENT_CAPACITY);
    *slots.events_rx = Some(rx);
    let token = CancellationToken::new();
    *slots.cancel = Some(token.clone());
    if bell == Bell::Ring {
        *slots.ring_on_completion = true;
    }
    let (tx_steer, steer_rx) = ilar::agent::steer_channel();
    *slots.steer_tx = Some(tx_steer);
    let resolver = deps.resolver.clone();
    let store = deps.store.clone();
    let session_id = deps.session_id.to_string();
    let system_prompt = deps.system_prompt.to_string();
    let registry = deps.registry.clone();
    let turn_ctx = deps.tool_ctx.clone();
    let mut loop_config = deps.loop_config.clone();
    // `/context`: the session's window beats whatever the resolver
    // reports, for this turn's compaction threshold. Read at spawn, so
    // a change mid-turn lands on the next one.
    if let Some(limit) = app.context_override {
        loop_config.context_limit = Some(limit);
    }
    *slots.handle = Some(tokio::spawn(async move {
        let result = match entry {
            RootTurn::New(text, images) => {
                run_turn(
                    resolver.as_ref(),
                    &registry,
                    &store,
                    &session_id,
                    &text,
                    &images,
                    Some(&system_prompt),
                    loop_config,
                    tx,
                    token,
                    turn_ctx,
                    Some(steer_rx),
                )
                .await
            }
            RootTurn::Resume => {
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
            RootTurn::Answer(response) => {
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
                .await
            }
        };
        let stranded = stranded_question(&store, &session_id);
        TurnCompletion::Root(result, stranded)
    }));
}

/// A detached /btw in flight: the question, and eventually its answer
/// (`Ok(None)` when cancelled or superseded).
type AsideHandle = tokio::task::JoinHandle<(String, Result<Option<String>>)>;

/// `settle`'s effectful edges over tokio and crossterm — see
/// schedule.rs for the order it is driven in. Constructed fresh each
/// iteration around mutable borrows of the loop's state, so a spawn
/// here is a spawn the rest of the iteration observes.
struct LoopRuntime<'a> {
    turn_handle: &'a mut Option<tokio::task::JoinHandle<TurnCompletion>>,
    /// A running /btw, beside the turn rather than in its slot; the
    /// question rides along for the answer modal.
    aside_handle: &'a mut Option<AsideHandle>,
    aside_cancel: &'a mut Option<CancellationToken>,
    events_rx: &'a mut Option<LoopEventReceiver>,
    cancel: &'a mut Option<CancellationToken>,
    steer_tx: &'a mut Option<ilar::agent::SteerSender>,
    pending_terminal_event: &'a mut Option<Event>,
    held_notifications: &'a mut std::collections::VecDeque<ilar::delivery::Parcel>,
    notifications: &'a mut tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    routed: &'a mut Vec<RoutedDelivery>,
    /// Names for sessions a delivery mentions, by id: a head read per
    /// unknown id, then remembered for the process.
    session_labels: &'a mut std::collections::HashMap<String, String>,
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
    /// Where the durable outbox lives, for retiring an entry whose
    /// delivery failed terminally and was salvaged into the transcript.
    outbox_dir: &'a std::path::Path,
    terminal: &'a mut ratatui::DefaultTerminal,
    bell_pending: &'a mut bool,
}

impl LoopRuntime<'_> {
    /// Start a root turn through the shared ritual. The question-answer
    /// resume lives in the key dispatch, outside any `LoopRuntime`, and
    /// calls [`spawn_root_turn`] with the loop's own locals instead.
    fn spawn_root_turn(&mut self, app: &mut App, entry: RootTurn, bell: Bell) {
        spawn_root_turn(
            app,
            TurnSlots {
                handle: &mut *self.turn_handle,
                events_rx: &mut *self.events_rx,
                cancel: &mut *self.cancel,
                steer_tx: &mut *self.steer_tx,
                ring_on_completion: &mut *self.ring_on_turn_completion,
            },
            TurnDeps {
                resolver: self.resolver,
                store: self.store,
                session_id: self.session_id,
                system_prompt: self.system_prompt,
                registry: self.registry,
                tool_ctx: self.tool_ctx,
                loop_config: self.loop_config,
            },
            entry,
            bell,
        );
    }
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
        let entry = match request {
            TurnRequest::New(text, images) => RootTurn::New(text, images),
            TurnRequest::Resume => RootTurn::Resume,
        };
        self.spawn_root_turn(app, entry, Bell::Ring);
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

    fn next_notification(&mut self) -> Option<ilar::delivery::Parcel> {
        next_notification(self.held_notifications, self.notifications)
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
            let services = registry.running_services();
            let result = ilar::compaction::compact_session(
                resolver.as_ref(),
                &store,
                &session_id,
                Some(&system_prompt),
                &tools,
                &services,
                &token,
            )
            .await;
            let stranded = stranded_question(&store, &session_id);
            TurnCompletion::Compaction(result, stranded)
        }));
    }

    fn start_aside(&mut self, app: &mut App, question: String) {
        // Detached, like topic titling: the turn slot, the busy state
        // and the queue all stay whose they were. A newer aside
        // replaces a still-running one — newest question wins.
        if let Some(previous) = self.aside_cancel.take() {
            previous.cancel();
        }
        app.set_notice("asking aside…", NoticeLevel::Info);
        let token = CancellationToken::new();
        *self.aside_cancel = Some(token.clone());
        let resolver = self.resolver.clone();
        let store = self.store.clone();
        let session_id = self.session_id.to_string();
        let system_prompt = self.system_prompt.to_string();
        let registry = self.registry.clone();
        *self.aside_handle = Some(tokio::spawn(async move {
            let tools = registry.definitions();
            let result = ilar::aside::ask(
                resolver.as_ref(),
                &store,
                &session_id,
                Some(&system_prompt),
                &tools,
                &question,
                &token,
            )
            .await;
            (question, result)
        }));
    }

    fn retire_notification(&mut self, notification: &ilar::subagent::Notification) {
        ilar::outbox::retire(self.outbox_dir, notification);
    }

    fn route(&mut self, _app: &mut App, parcel: ilar::delivery::Parcel) {
        // Detached, like an aside: the delivery resumes another
        // session, so the turn slot, the busy state and the activity
        // all stay whose they were. Several may run at once; the
        // session claim serializes deliveries to the same child. No
        // notice: the agents panel shows the delivering row for as
        // long as it runs, and the ending says where it went.
        let token = CancellationToken::new();
        let spawner = self.spawner.clone();
        let delivered = parcel.notification().clone();
        let cancel = token.clone();
        let handle =
            tokio::spawn(async move { spawner.route_notification(delivered, cancel).await });
        self.routed.push(RoutedDelivery {
            handle,
            cancel: token,
            parcel,
        });
    }

    fn steer_notification(
        &mut self,
        app: &mut App,
        parcel: ilar::delivery::Parcel,
    ) -> Option<ilar::delivery::Parcel> {
        // The same rails as a user steer: sent into the live channel,
        // tracked as pending so a turn that ends without reading it
        // splices it into the queue instead of losing it.
        let Some(tx) = self.steer_tx.as_ref() else {
            return Some(parcel);
        };
        let message = ilar::agent::Steer {
            text: parcel.notification().text.clone(),
            images: Vec::new(),
        };
        if tx.send(message.clone()).is_err() {
            // The channel closed under us — the turn is ending; the
            // notification goes back to be held for the next pass.
            return Some(parcel);
        }
        app.pending_steers.push(message);
        None
    }

    fn start_notification_turn(
        &mut self,
        app: &mut App,
        notification: ilar::subagent::Notification,
    ) {
        let text = notification.text;
        app.push_notification(&notification.description, &text);
        app.busy = true;
        app.turn_committed = false;
        app.retry_available = false;
        app.status = "thinking".into();
        app.clear_transient_notice();
        app.set_activity(Activity::Thinking);
        // The loop started this one on its own: no bell when it lands.
        self.spawn_root_turn(app, RootTurn::New(text, Vec::new()), Bell::Silent);
    }

    fn session_id(&self) -> &str {
        self.session_id
    }

    fn session_label(&mut self, app: &App, session_id: &str) -> String {
        session_label(
            app,
            self.store,
            self.session_id,
            self.session_labels,
            session_id,
        )
    }

    fn resume_notifications(&mut self) {
        *self.notifications_paused = false;
    }

    fn pause_notifications(&mut self) {
        *self.notifications_paused = true;
    }

    fn hold_propagate(&mut self, parcel: ilar::delivery::Parcel) {
        hold_propagate_behind_backlog(self.held_notifications, self.notifications, parcel);
    }

    fn hold_requeue(&mut self, parcel: ilar::delivery::Parcel) {
        self.held_notifications.push_front(parcel);
    }

    fn hold_blocked(&mut self, parcels: Vec<ilar::delivery::Parcel>) {
        for parcel in parcels.into_iter().rev() {
            self.held_notifications.push_front(parcel);
        }
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
            Ok(_) => {
                app.current_model = model.clone();
                app.current_variant = variant;
                app.set_model_context_limit(display_context_limit(self.resolver.as_ref(), &model));
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
        // Checked here rather than left to the task tool, whose
        // refusal is addressed to a model: it names `subagent_type`,
        // a JSON field nobody typed, instead of the `agent:` line in
        // the file the user wrote.
        let available: Vec<&str> = self
            .spawner
            .agents()
            .iter()
            .map(|agent| agent.name.as_str())
            .collect();
        if let Some(refusal) = unknown_subtask_agent(&request.agent, &available) {
            let message = format!("{description}: {refusal}");
            app.set_notice(message.clone(), NoticeLevel::Error);
            app.push_transcript_line(Line_::System(message));
            return;
        }
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
            // The transcript line and the agents panel both say so; the
            // notice line stays free.
            app.push_transcript_line(Line_::System(format!(
                "{description} running in the background as {} — its result will land here when it finishes",
                request.agent
            )));
        }
    }

    fn present(&mut self, app: &mut App) -> Result<()> {
        let _ = ring_terminal_bell_if_idle(
            &mut std::io::stdout(),
            self.bell_pending,
            self.turn_handle.is_some(),
        );
        // Deliveries count as background work: they are jobs the user
        // may want to wait for or cancel, and the pending manager's
        // cancel-all takes them too.
        app.background_running = self.spawner.running_background() + self.routed.len();
        app.deliveries_in_flight = self.routed.len();
        // A pause stops the drain, so a completion that lands during
        // one stays unread in the channel: invisible on the notice
        // row, absent from the pending manager, uncounted by the quit
        // warning — while the abort that paused things promised its
        // result was held. Move it into the backlog that is actually
        // shown.
        if *self.notifications_paused {
            drain_into_backlog(self.held_notifications, self.notifications);
        }
        // Headlines, not a count: the pending manager lists which
        // results wait and offers to deliver them.
        app.held_results = self
            .held_notifications
            .iter()
            .map(|parcel| {
                let notification = parcel.notification();
                crate::app::notification_headline(&notification.text)
                    .unwrap_or_else(|| notification.description.clone())
            })
            .collect();
        app.notifications_paused = *self.notifications_paused;
        let tasks = self.spawner.running_tasks();
        // Depths from the registry's own ancestry, in registry order:
        // children stay after their parent, roots keep their place.
        let depths = decide::tree_depths(
            &tasks
                .iter()
                .map(|task| (task.session_id.clone(), task.parent_session_id.clone()))
                .collect::<Vec<_>>(),
        );
        app.agents_view = tasks
            .into_iter()
            .zip(depths)
            .map(|(task, depth)| AgentRow {
                // An indented row already names its parent by position;
                // the "for {id}" note is for foreign roots the panel
                // cannot indent under anyone.
                foreign_parent: (depth == 0
                    && !task.parent_session_id.is_empty()
                    && task.parent_session_id != *self.session_id)
                    .then(|| {
                        // Named, and kept short: the note shares its
                        // row with the agent and the elapsed time.
                        let name = self
                            .session_labels
                            .entry(task.parent_session_id.clone())
                            .or_insert_with(|| session_name(self.store, &task.parent_session_id));
                        crate::text::truncate_display(
                            name,
                            FOREIGN_PARENT_CHARS,
                            crate::text::Truncation::Middle,
                        )
                    }),
                session_id: task.session_id,
                depth,
                description: task.description,
                agent: task.agent,
                background: task.background,
                delivering: task.delivering,
                elapsed: task.started.elapsed(),
                waiting: task.waiting,
                quiet: task.quiet,
            })
            .collect();
        app.services_view = self.services.snapshot();
        app.services_running = app
            .services_view
            .iter()
            .filter(|(_, running, _)| *running)
            .count();
        if std::mem::take(&mut app.force_full_redraw) {
            self.terminal.clear()?;
        }
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

/// Open the focus view on a child session: a placeholder immediately —
/// the roster row lends the title and says the agent is running — and
/// the seed follows from a blocking worker, because a large child's
/// replay used to freeze the UI for the length of its log. Returns
/// whether the child is streaming, which the seed needs — or `None`
/// when the view was already on that agent and there is nothing to do.
fn open_agent_focus(app: &mut App, store: &SessionStore, session_id: &str) -> Option<bool> {
    // A second click on the row already in front is not a navigation:
    // closing and reopening would round-trip the drafts, stashing what
    // was typed at this agent and handing the root's back.
    if app
        .focus
        .as_ref()
        .is_some_and(|focus| focus.session_id == session_id)
    {
        return None;
    }
    // A click from inside another focus view: that one hands the prompt
    // back first, so the root's own draft cannot be swallowed by the
    // second view parking what was typed at the first agent.
    app.close_focus();
    let roster = app
        .agents_view
        .iter()
        .find(|row| row.session_id == session_id);
    let title = match roster {
        Some(row) => format!("{} · {}", row.agent, row.description),
        None => session_name(store, session_id),
    };
    let running = roster.is_some();
    // A delivering row is a routed completion being handed to a
    // session, not a turn streaming into this process: it publishes no
    // activity at all, so a row the seed left open would spin forever.
    // Only an agent whose events will actually arrive gets that.
    let streaming = roster.is_some_and(|row| !row.delivering);
    // What Enter may do here, judged now: the roster row is gone the
    // moment the agent finishes, and a refusal that depends on whose
    // child it is must not go with it. Without a row the log says whose
    // it is — `message_task` refuses anything but this session's own
    // children, and it does so in the model's words with a uuid in them.
    let unreachable = match roster {
        Some(row) => match &row.foreign_parent {
            Some(owner) => Some(format!("{owner}'s agent, not this session's")),
            None if row.depth > 0 => Some("another agent's child, not this session's".to_string()),
            None => None,
        },
        None => {
            let parent = store
                .head(session_id)
                .ok()
                .and_then(|head| head.meta.parent_id);
            (parent.as_deref() != Some(app.session_id.as_str()))
                .then(|| "not a task this session started".to_string())
        }
    };
    let foreground = roster.is_some_and(|row| !row.background && !row.delivering);
    // A live search would keep the keyboard and make the focus
    // footer lie; the click is a navigation, so the search is over.
    app.close_search(false);
    // The root's draft goes with the root: the prompt now belongs to
    // the agent on screen, and Esc hands it back.
    let parked = crate::app::StashedPrompt {
        text: app.input.take(),
        images: std::mem::take(&mut app.pending_images),
    };
    app.focus = Some(FocusView {
        unreachable,
        foreground,
        parked,
        ..FocusView::new(
            session_id.to_string(),
            title,
            vec![crate::transcript::Line_::System(
                "loading transcript…".into(),
            )],
            running,
        )
    });
    Some(streaming)
}

/// The focus seed, built off the UI task: the store replay — reading
/// is safe while the child's turn holds the writer lock, the picker
/// preview does the same. With-store, so a child's own subagents
/// bring their history along instead of empty nested timelines.
fn seed_agent_focus(
    store: &SessionStore,
    session_id: &str,
    streaming: bool,
) -> Result<Vec<crate::transcript::Line_>, String> {
    let reader = store
        .load(session_id)
        .map_err(|error| format!("cannot open agent transcript: {error:#}"))?;
    // A working agent's open tool rows are open, not failed: marking
    // them ✗ here also made the real result unsettleable, so the row
    // lied until the next refocus.
    let mut restored = session_view::restored_session_view_with_store(
        &reader,
        store,
        if streaming {
            session_view::Liveness::Running
        } else {
            session_view::Liveness::Settled
        },
    );
    if streaming {
        // The store commits step by step: whatever the session has
        // streamed since its last step boundary is not in the seed.
        // Say so — and end the seed on a non-text line, so the next
        // delta starts fresh instead of welding onto an older
        // paragraph.
        restored.lines.push(crate::transcript::Line_::System(
            "focused mid-turn — the step in flight joins live from here".into(),
        ));
    }
    Ok(restored.lines)
}

/// Land a finished seed on the focus that asked for it. A focus
/// closed or retargeted while the worker ran drops the seed; a session
/// that would not load is a notice, never a blank screen.
fn land_agent_focus(
    app: &mut App,
    for_session: &str,
    seeded: Result<Vec<crate::transcript::Line_>, String>,
) {
    if !app
        .focus
        .as_ref()
        .is_some_and(|focus| focus.session_id == for_session)
    {
        return;
    }
    match seeded {
        Ok(lines) => {
            let focus = app.focus.as_mut().expect("checked above");
            focus.replace_lines(lines);
        }
        Err(message) => {
            // The close may stash something typed while the seed
            // replayed; its notice would be overwritten by this one, so
            // the two share the line.
            let stashed = app.close_focus();
            let stashed = if stashed {
                " — your unsent message was stashed"
            } else {
                ""
            };
            app.set_notice(format!("{message}{stashed}"), NoticeLevel::Error);
        }
    }
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

struct MotionBatch {
    column: u16,
    row: u16,
    deferred: Option<Event>,
}

/// Motion tracking emits an event per cell crossed; a sweep would
/// otherwise cost one frame per event, and a click behind the flood
/// would wait its turn. Collapse a run to its newest position, like
/// the wheel batch.
fn drain_motion_batch(
    initial: (u16, u16),
    max_events: usize,
    mut try_next: impl FnMut() -> Result<Option<Event>>,
) -> Result<MotionBatch> {
    let (mut column, mut row) = initial;
    let mut deferred = None;
    let mut events = 1usize;
    while events < max_events.max(1) {
        let Some(event) = try_next()? else {
            break;
        };
        match event {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: next_column,
                row: next_row,
                ..
            }) => {
                column = next_column;
                row = next_row;
                events += 1;
            }
            other => {
                deferred = Some(other);
                break;
            }
        }
    }
    Ok(MotionBatch {
        column,
        row,
        deferred,
    })
}

/// Whether a command's `agent:` names an agent that exists, and what
/// to say when it does not. The task tool's own refusal is written for
/// a model — `unknown subagent_type "nope"; available: build, explore`
/// — and naming a JSON field the user never typed leaves them looking
/// for it. This names the frontmatter key they did write.
fn unknown_subtask_agent(agent: &str, available: &[&str]) -> Option<String> {
    if available.contains(&agent) {
        return None;
    }
    Some(format!(
        "no agent named {agent:?} — this command's `agent:` must be one of: {}",
        available.join(", ")
    ))
}

/// What Esc says when it aborts a turn. A detached task's cancellation
/// token is a child of the turn's, so aborting the turn stops the tasks
/// that turn started; their results are held rather than delivered,
/// because the abort pauses notifications the way cancel-all does.
/// `detached` is the count of background tasks running at all — a
/// superset of the ones this turn owns — so the notice never names a
/// number, only the rule.
fn abort_notice(detached: usize) -> String {
    if detached == 0 {
        "aborting current operation…".into()
    } else {
        "aborting current operation… — detached tasks this turn started stop with it; \
         their results are held until your next message"
            .into()
    }
}

/// The reasons a session switch (resume, fork, rewind) must wait; the
/// same set guards every path that tears the runtime down. The stash is
/// not among them: it rides along into the rebuilt app, so there is
/// nothing to lose by leaving.
///
/// A goal is among them. It lives in the running app and in nothing
/// else — no event records it, and neither `App::new` nor a switch
/// carries it — so leaving would drop the goal, its round budget and
/// its badge without a word. `/rewind` refused for that reason; every
/// other way out refused for none, so the refusal lives here and the
/// user ends the goal deliberately (which says so in the transcript).
fn switch_blocked(
    turn_running: bool,
    background_agents: usize,
    deliveries: usize,
    has_draft: bool,
    goal_active: bool,
) -> Option<String> {
    if turn_running {
        Some("finish or abort the current turn before switching sessions".into())
    } else if background_agents > 0 {
        // Naming the keys: "abort them first" sent the user looking for
        // a cancel that was only reachable through Ctrl-Q's `d d`, and
        // said nothing about stopping just the one in the way. Kept
        // short: this string is also used as a notice line, where an
        // 80-column terminal would truncate the second key away.
        Some(format!(
            "{background_agents} background task(s) running; Ctrl-Q cancels all, Ctrl-G one"
        ))
    } else if deliveries > 0 {
        Some("a task result is being delivered; wait a moment".into())
    } else if has_draft {
        Some("input has an unsent draft; send or clear it first".into())
    } else if goal_active {
        Some("a goal is active — /goal abort before leaving its context".into())
    } else {
        None
    }
}

/// How run_app ended: quit the program, or restart against another session.
enum AppExit {
    Quit,
    /// Switch carrying what must survive the rebuild: a rewind or fork
    /// prefill, its notice, and the stash — which belongs to the person,
    /// not to the session they happened to be in.
    SwitchInto {
        id: String,
        prefill: Option<String>,
        notice: Option<String>,
        stash: Vec<crate::app::StashedPrompt>,
    },
}

/// Hits shown per session in the content search; one chatty session
/// must not bury the rest.
const SEARCH_HITS_PER_SESSION: usize = 5;
/// Events either side of a hit carried into the preview pane.
const SEARCH_CONTEXT_RADIUS: usize = 2;
/// Characters of any one entry in the preview.
const SEARCH_CONTEXT_CHARS: usize = 500;

/// One row of the resume listing. Nothing is read: a summary already
/// carries everything the row shows, and the launch directory is
/// compared exactly as recorded — the same comparison `--continue`
/// makes, so the two surfaces cannot disagree about what "here" is.
fn listing_row(
    summary: &ilar::session::SessionSummary,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
    now: std::time::SystemTime,
) -> SearchRow {
    SearchRow {
        title: summary
            .title
            .clone()
            .unwrap_or_else(|| UNTITLED_SESSION.to_string()),
        untitled: summary.title.is_none(),
        session_id: summary.id.clone(),
        event: 0,
        age: crate::modals::last_used(summary.modified, now),
        origin: crate::modals::row_origin(summary.cwd.as_deref(), Some(cwd), home),
        match_count: 0,
        title_match: false,
        context: Vec::new(),
    }
}

/// Start a background walk of every session for the search modal: a
/// content grep when there is a query, the recent-sessions listing
/// when there is not. Rows stream through the channel as each session
/// is read; setting the returned flag abandons the walk at the next
/// session boundary.
fn start_session_scan(
    store: ilar::session::SessionStore,
    query: String,
    current_session: String,
    // The directory rows are grouped against — the workspace's own
    // canonical cwd, the same value a session records in its meta.
    cwd: std::path::PathBuf,
) -> (
    std::sync::mpsc::Receiver<Vec<SearchRow>>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let now = std::time::SystemTime::now();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        // Symlinked launch paths must still count as "here".
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        if query.trim().is_empty() {
            // This directory's last session, straight from the pointer
            // file: one JSON read and one head read, sent before the
            // listing so the modal opens on the right row even while
            // the scan is still running — and even with a cold cache.
            let pointed = store
                .last_in(&cwd)
                // Resuming into yourself is not a switch.
                .filter(|summary| summary.id != current_session);
            if let Some(summary) = &pointed {
                let _ = tx.send(vec![listing_row(summary, &cwd, home.as_deref(), now)]);
            }
            if flag.load(Ordering::Relaxed) {
                return;
            }
            // The resume listing: head scans only (and the summary cache
            // answers most of those), one batch, already newest-first —
            // the modal keeps this directory's sessions at the top and
            // the preview loads lazily. This is what makes the modal
            // open instantly: nothing here reads a session past its
            // head.
            //
            // The cap is per group, and that is the whole point: taking
            // the newest 200 sessions *first* left this directory's last
            // session off the list entirely once 200 sessions elsewhere
            // were newer.
            let (mine, others): (Vec<SearchRow>, Vec<SearchRow>) = store
                .list()
                .into_iter()
                .filter(|summary| {
                    summary.id != current_session
                        && pointed.as_ref().is_none_or(|row| row.id != summary.id)
                })
                .map(|summary| listing_row(&summary, &cwd, home.as_deref(), now))
                .partition(|row| row.origin.here());
            let rows: Vec<SearchRow> = mine
                .into_iter()
                .take(MAX_SEARCH_ROWS)
                .chain(others.into_iter().take(MAX_SEARCH_ROWS))
                .collect();
            let _ = tx.send(rows);
            return;
        }
        // Query mode: one row per matching session, its best hit
        // centered in the preview context. Sessions are read one at a
        // time and stream in newest-first; the modal ranks title
        // matches ahead as they arrive. The listing is taken once and
        // handed to the walk — it used to be read twice per search.
        let sessions = store.list();
        let launched_in: std::collections::HashMap<&str, Option<&std::path::Path>> = sessions
            .iter()
            .map(|session| (session.id.as_str(), session.cwd.as_deref()))
            .collect();
        let needle = query.to_lowercase();
        let mut sent = 0usize;
        ilar::recall::search_sessions(
            &store,
            &sessions,
            &query,
            SEARCH_HITS_PER_SESSION,
            &flag,
            |entries, hits| {
                if flag.load(Ordering::Relaxed) {
                    return false;
                }
                let Some(best) = hits.hits.first() else {
                    return true;
                };
                let title = hits
                    .title
                    .clone()
                    .unwrap_or_else(|| UNTITLED_SESSION.to_string());
                // As recorded, uncanonicalized: the same comparison the
                // resume listing and `--continue` make.
                let launched = launched_in.get(hits.session_id.as_str()).copied().flatten();
                let context = ilar::recall::around(
                    entries,
                    best.event,
                    SEARCH_CONTEXT_RADIUS,
                    SEARCH_CONTEXT_CHARS,
                )
                .into_iter()
                .map(|entry| {
                    let is_hit = entry.event == best.event;
                    let text = if is_hit {
                        // The slice `around` took runs from the front;
                        // the match may live past it. Re-center the hit
                        // entry on the match so the reason this row
                        // exists is always on screen.
                        entries
                            .iter()
                            .find(|original| original.event == best.event)
                            .map(|original| {
                                center_on_match(&original.text, &needle, SEARCH_CONTEXT_CHARS)
                            })
                            .unwrap_or(entry.text)
                    } else {
                        entry.text
                    };
                    (entry.speaker.label().to_string(), text, is_hit)
                })
                .collect();
                let row = SearchRow {
                    session_id: hits.session_id.clone(),
                    title_match: title.to_lowercase().contains(&needle),
                    untitled: hits.title.is_none(),
                    title,
                    event: best.event,
                    age: crate::modals::last_used(hits.modified, now),
                    origin: crate::modals::row_origin(launched, Some(&cwd), home.as_deref()),
                    match_count: hits.hits.len(),
                    context,
                };
                sent += 1;
                tx.send(vec![row]).is_ok() && sent < MAX_SEARCH_ROWS
            },
        );
    });
    (rx, cancel)
}

/// A window of `max_chars` centered on the first case-insensitive
/// occurrence of `needle` (already lowercased) in `text`, elision
/// marked on the cut ends. Lowercasing can shift byte offsets on some
/// scripts, so the mapping is by char count and the window clamps —
/// worst case the match sits off-center, never off-screen.
fn center_on_match(text: &str, needle: &str, max_chars: usize) -> String {
    let lowered = text.to_lowercase();
    let Some(byte) = lowered.find(needle) else {
        return text.chars().take(max_chars).collect();
    };
    let match_chars = lowered[..byte].chars().count();
    let total = text.chars().count();
    let start = match_chars
        .saturating_sub(max_chars / 2)
        .min(total.saturating_sub(max_chars));
    let mut window: String = text.chars().skip(start).take(max_chars).collect();
    if start > 0 {
        window.insert(0, '…');
    }
    if start + max_chars < total {
        window.push('…');
    }
    window
}

/// The terminal window's title: the session's topic once it has one.
fn terminal_title(topic: Option<&str>) -> String {
    match topic {
        Some(topic) => format!("ilar — {topic}"),
        None => "ilar".into(),
    }
}

/// Best-effort OSC title update; a terminal that ignores the sequence
/// simply keeps its own title, and the shell's prompt hook takes the
/// window back after exit.
fn apply_terminal_title(topic: Option<&str>) {
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::SetTitle(terminal_title(topic))
    );
}

/// The lazy preview load for the search modal's listing rows:
/// (generation, session id, the loader's channel).
type PreviewTask = (
    u64,
    String,
    std::sync::mpsc::Receiver<Vec<(String, String, bool)>>,
);
/// A focus seed replaying on a worker: which child it is for, and the
/// handle carrying its lines.
type FocusSeedTask = (
    String,
    tokio::task::JoinHandle<Result<Vec<crate::transcript::Line_>, String>>,
);
/// A rewind in flight: the discarded-turn count for its notice, the
/// pause state to restore on failure (the hold may predate the rewind),
/// and the handle.
type RewindTask = (
    usize,
    bool,
    tokio::task::JoinHandle<Result<ilar::rewind::RewindReport>>,
);

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
    adoption_handle: tokio::task::JoinHandle<Vec<ilar::subagent::Notification>>,
    mut notifications: tokio::sync::mpsc::Receiver<ilar::subagent::Notification>,
    mut subagent_activity: tokio::sync::broadcast::Receiver<ilar::subagent::SubagentActivity>,
    mut question_rx: ilar::question::QuestionReceiver,
    mut ask_rx: ilar::secrets::AskReceiver,
    loop_config: LoopConfig,
    model_choices: Vec<&'static ilar::model::ModelInfo>,
    services: std::sync::Arc<ilar::tools::service::ServiceManager>,
    outbox_dir: std::path::PathBuf,
    initial_pending_question_id: Option<String>,
    mut restore_handle: Option<(
        usize,
        tokio::task::JoinHandle<(session_view::RestoredSessionView, u64)>,
    )>,
) -> Result<AppExit> {
    let mut events_rx: Option<LoopEventReceiver> = None;
    // The content-search scan: rows stream in stamped with the query
    // generation they answer; the flag abandons a stale walk.
    let mut search_rx: Option<(u64, std::sync::mpsc::Receiver<Vec<SearchRow>>)> = None;
    // One in flight; the newest selection wins.
    let mut preview_rx: Option<PreviewTask> = None;
    let mut search_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    // Seeded empty: the outbox scan is still running on its worker.
    // What it recovers lands mid-loop through `adoption_handle` —
    // held, and paused unless the user has engaged by then.
    let mut held_notifications: std::collections::VecDeque<ilar::delivery::Parcel> =
        std::collections::VecDeque::new();
    let mut notifications_paused = false;
    let mut adoption_handle = Some(adoption_handle);
    // Whether the user has started a turn this run. Before the first
    // turn no children exist, so any running turn is the user's own
    // doing — and a backlog landing after that point belongs to
    // someone who is present, not to someone reading.
    let mut engaged = false;
    let mut focus_seed: Option<FocusSeedTask> = None;
    // The loop keeps drawing while git restores the tree; notifications
    // are paused for the duration so nothing writes the log mid-rewrite.
    let mut rewind_task: Option<RewindTask> = None;
    // Deliveries to other sessions, running beside the turn slot.
    let mut routed: Vec<RoutedDelivery> = Vec::new();
    let mut focus_messages: Vec<FocusMessage> = Vec::new();
    let mut session_labels = std::collections::HashMap::new();
    let mut cancel: Option<CancellationToken> = None;
    // Live only while a root turn runs, so a message typed during that
    // turn is steered into it. Cross-session routed turns have no
    // channel and still queue.
    let mut steer_tx: Option<ilar::agent::SteerSender> = None;
    let mut turn_handle: Option<tokio::task::JoinHandle<TurnCompletion>> = None;
    let mut topic_handle: Option<tokio::task::JoinHandle<Option<String>>> = None;
    // A running /btw, detached from the turn slot entirely.
    let mut aside_handle: Option<AsideHandle> = None;
    let mut aside_cancel: Option<CancellationToken> = None;
    let mut ring_on_turn_completion = false;
    let mut bell_pending = false;
    // The stall watchdog's bell fires once per silence episode; data
    // arriving (or the clock stopping) re-arms it.
    let mut stall_bell_rung = false;
    let mut pending_terminal_event = None;
    let mut question_reply: Option<tokio::sync::oneshot::Sender<ilar::question::QuestionResponse>> =
        None;
    let mut pending_question_id = initial_pending_question_id;
    // The open prompts' reply paths. Nothing persists: a grant is for
    // the command in front of the person, and the tool is blocked on it
    // until they answer or the turn goes away. The password is its own
    // ask, so its own path.
    let mut grant_reply: Option<tokio::sync::oneshot::Sender<Option<ilar::secrets::Grant>>> = None;
    let mut password_reply: Option<tokio::sync::oneshot::Sender<Option<String>>> = None;
    // Decisions accumulate here and are performed in one place below,
    // rather than each arm doing its own effects inline.
    let mut intents: Vec<Intent> = Vec::new();
    // What the last completed turn said about an unanswered question
    // in the log. Turns read this on their own task; the loop only
    // pays a parse on the crash path below, which reported nothing.
    let mut stranded_question_report: Option<ilar::session::PendingQuestion> = None;
    let mut recheck_pending_question = false;

    // Name the window like the transcript header: the topic when the
    // session has one, updated again if titling lands mid-run.
    apply_terminal_title(app.topic.as_deref());

    loop {
        // A failed/cancelled resume may leave the persisted question pending.
        // Reopen it instead of stranding the session behind a rejected new turn.
        if std::mem::take(&mut recheck_pending_question) {
            stranded_question_report = stranded_question(store, session_id);
        }
        if turn_handle.is_none()
            && app.question_modal.is_none()
            && let Some(pending) = stranded_question_report.take()
        {
            pending_question_id = Some(pending.tool_call_id.clone());
            question_reply = None;
            app.question_modal = Some(questions::QuestionModal::new(pending.request));
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
        // The root turn's stall watchdog, judged on the freshly drained
        // clock. Only a root turn is watched — compaction and routed
        // deliveries own no events_rx — and only while it should be
        // streaming: a question pause and an abort already in flight
        // stop the clock, a running tool holds it (the verdict's
        // business). Warn generously, ring once, and only abort — via
        // the same token as Esc — after ROOT_STALL_ABORT_AFTER of
        // literal nothing.
        let watched = turn_handle.is_some()
            && events_rx.is_some()
            && !matches!(app.activity, Activity::Paused | Activity::Aborting);
        let silence = if watched {
            app.stream_last_data.map(|last| last.elapsed())
        } else {
            None
        };
        match decide::stall_verdict(
            silence,
            app.activity == Activity::Tools,
            ROOT_STALL_WARN_AFTER,
            ROOT_STALL_ABORT_AFTER,
        ) {
            decide::StallVerdict::Quiet => stall_bell_rung = false,
            decide::StallVerdict::Warn { silent_secs } => {
                // Persistent, so the stream of passes can keep the count
                // climbing; any loop event — data at last — clears it
                // (`App::push_loop_event`). Guarded: it may not bury a
                // standing persistent reminder that data would then
                // destroy along with it.
                app.set_stall_notice(format!(
                    "provider silent for {silent_secs}s — Esc aborts, Ctrl-R then resumes the turn"
                ));
                if !std::mem::replace(&mut stall_bell_rung, true) {
                    use std::io::Write as _;
                    // Directly, not via bell_pending: that one waits for
                    // an idle turn, and the point here is mid-turn.
                    let mut out = std::io::stdout();
                    let _ = out.write_all(b"\x07").and_then(|()| out.flush());
                }
            }
            decide::StallVerdict::Abort { silent_secs } => {
                if let Some(cancel) = &cancel {
                    let message = format!(
                        "stall watchdog: provider silent for {silent_secs}s — aborting the turn"
                    );
                    // The transcript keeps the why; the TurnDone the
                    // cancellation produces closes the rows and posts
                    // the ordinary "turn aborted".
                    app.push_transcript_line(crate::transcript::Line_::System(message.clone()));
                    app.set_stall_notice(message);
                    cancel.cancel();
                    app.status = "aborting…".into();
                    app.set_activity(Activity::Aborting);
                }
            }
        }
        // Stream in content-search rows; a closed channel means the
        // scan finished (or was told to stop, which no longer matters).
        let mut scan_done = false;
        if let Some((generation, rx)) = search_rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(rows) => {
                        if let Some(search) = app.session_search.as_mut() {
                            search.push_rows(*generation, rows);
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if let Some(search) = app.session_search.as_mut() {
                            search.finish_scan(*generation);
                        }
                        scan_done = true;
                        break;
                    }
                }
            }
        }
        if scan_done {
            search_rx = None;
        }
        // The listing carries no context — load the selected session's
        // tail the first time it is looked at, in the background, and
        // fill the row when it lands. A stale load (selection moved on,
        // query changed) fills nothing and is simply replaced.
        if let Some(search) = app.session_search.as_ref()
            && let Some(row) = search.selected()
            && row.context.is_empty()
        {
            let wanted = (search.generation, row.session_id.clone());
            let in_flight = preview_rx
                .as_ref()
                .is_some_and(|(generation, sid, _)| *generation == wanted.0 && *sid == wanted.1);
            if !in_flight {
                let (tx, rx) = std::sync::mpsc::channel();
                let store = store.clone();
                let sid = wanted.1.clone();
                tokio::task::spawn_blocking(move || {
                    // Always answer — a load that sent nothing would
                    // leave the row empty and this loop re-spawning a
                    // loader every pass.
                    let context = ilar::recall::session_entries(&store, &sid)
                        .ok()
                        .and_then(|entries| {
                            let last = entries.last()?;
                            Some(
                                ilar::recall::around(
                                    &entries,
                                    last.event,
                                    SEARCH_CONTEXT_RADIUS,
                                    SEARCH_CONTEXT_CHARS,
                                )
                                .into_iter()
                                .map(|entry| (entry.speaker.label().to_string(), entry.text, false))
                                .collect::<Vec<_>>(),
                            )
                        })
                        .unwrap_or_else(|| {
                            vec![(String::new(), "preview unavailable".into(), false)]
                        });
                    let _ = tx.send(context);
                });
                preview_rx = Some((wanted.0, wanted.1, rx));
            }
        }
        if let Some((generation, sid, rx)) = preview_rx.as_ref() {
            match rx.try_recv() {
                Ok(context) => {
                    if let Some(search) = app.session_search.as_mut()
                        && search.generation == *generation
                    {
                        for row in search
                            .rows
                            .iter_mut()
                            .filter(|row| row.session_id == *sid && row.context.is_empty())
                        {
                            row.context = context.clone();
                        }
                    }
                    preview_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => preview_rx = None,
            }
        }
        // A search modal that wants a scan and has none running gets
        // one — this serves both the just-opened modal (whoever opened
        // it) and the keystroke that cancelled the previous scan.
        if let Some(search) = app.session_search.as_ref()
            && search.scanning
            && search_rx.is_none()
        {
            let (rx, flag) = start_session_scan(
                store.clone(),
                search.query.clone(),
                app.session_id.clone(),
                tool_ctx.location.cwd().to_path_buf(),
            );
            search_rx = Some((search.generation, rx));
            search_cancel = Some(flag);
        }
        for _ in 0..ilar::subagent::ACTIVITY_CAPACITY {
            match subagent_activity.try_recv() {
                Ok(activity) => {
                    app.push_subagent_activity(&activity);
                    // The focus view reads the same feed, beside the
                    // root's nested previews, never instead of them.
                    app.push_focus_activity(&activity);
                }
                // A lag is a gap, not an end: the feed dropped older
                // events, and everything after them is still waiting.
                // Breaking here stalled the tape for a whole frame each
                // time several children streamed at once — precisely
                // when it had the most to show.
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        // Once per frame, after the drain: an activity whose parent tool
        // row had not appeared yet is held, and the row it was waiting
        // for may have arrived in this very batch. Retrying inside the
        // fold instead — once per event, over a queue of up to 256 —
        // made a busy frame quadratic in the transcript's length.
        app.retry_subagent_activity();
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
        // A grant whose asker stopped listening — the turn ended or
        // was aborted under the modal — closes without an answer; the
        // prompt must not outlive the command it named. It leaves a
        // line behind: a modal that simply vanished looked like a
        // keystroke of the user's had answered it, and the status stayed
        // on "waiting for your grant" until the next event.
        let withdrawn = if app.grant_modal.is_some()
            && grant_reply.as_ref().is_none_or(|reply| reply.is_closed())
        {
            grant_reply = None;
            app.grant_modal.take().map(|modal| modal.withdrawn_line())
        } else if app.password_modal.is_some()
            && password_reply
                .as_ref()
                .is_none_or(|reply| reply.is_closed())
        {
            password_reply = None;
            app.password_modal
                .take()
                .map(|modal| modal.withdrawn_line())
        } else {
            None
        };
        if let Some(line) = withdrawn {
            app.push_transcript_line(Line_::System(line));
            if turn_handle.is_some() {
                app.status = "thinking".into();
                app.set_activity(Activity::Thinking);
            } else {
                app.status = "ready".into();
                app.set_activity(Activity::Ready);
            }
        }
        // One prompt at a time: a second asker waits in the channel
        // until this one is answered, so no reply is dropped unread.
        // Unlike a question, a child's ask is not filtered out: the
        // channel is this runtime's alone, a subagent's bash needs the
        // same yes, and dropping it would be a silent refusal. The
        // modal names the asker instead.
        if app.grant_modal.is_none()
            && app.password_modal.is_none()
            && let Ok(ask) = ask_rx.try_recv()
        {
            let from_subagent = ask.session_id() != session_id;
            match ask {
                ilar::secrets::Ask::Grant(prompt) => {
                    app.grant_modal = Some(grants::GrantModal::new(&prompt, from_subagent));
                    grant_reply = Some(prompt.reply);
                    app.status = "waiting for your grant".into();
                }
                ilar::secrets::Ask::Password(prompt) => {
                    app.password_modal = Some(grants::PasswordModal::new(&prompt, from_subagent));
                    password_reply = Some(prompt.reply);
                    app.status = "waiting for the sudo password".into();
                }
            }
            app.set_activity(Activity::Paused);
        }
        // Rewind and fork requests recorded by /rewind, /fork or the
        // palette; they need the store, so they are consumed here.
        if std::mem::take(&mut app.turn_picker_requested) {
            if let Some(reason) = switch_blocked(
                turn_handle.is_some(),
                spawner.running_background(),
                routed.len(),
                !app.input.is_blank(),
                app.goal.is_some(),
            ) {
                app.set_notice(reason, NoticeLevel::Warning);
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
                routed.len(),
                !app.input.is_blank(),
                app.goal.is_some(),
            ) {
                app.set_notice(reason, NoticeLevel::Warning);
            } else {
                match store.fork(session_id) {
                    Ok(fork_id) => {
                        leave_session(
                            &spawner,
                            &mut aside_cancel,
                            &mut aside_handle,
                            &mut topic_handle,
                        )
                        .await;
                        return Ok(AppExit::SwitchInto {
                            id: fork_id,
                            prefill: None,
                            notice: Some(format!(
                                "forked from {}",
                                session_name(store, session_id)
                            )),
                            stash: std::mem::take(&mut app.input_stash),
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
        let mut completions = Vec::new();
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
            completions.push(match handle.await {
                Ok(TurnCompletion::Root(result, stranded)) => {
                    // Latest truth wins: an answer turn that consumed
                    // the question overwrites the report that opened
                    // it.
                    stranded_question_report = stranded;
                    schedule::Completion::Root(result)
                }
                Ok(TurnCompletion::Compaction(result, stranded)) => {
                    stranded_question_report = stranded;
                    schedule::Completion::Compaction(result)
                }
                Err(error) => {
                    // A panicked task reported nothing; only this
                    // path still pays a load at the loop top.
                    recheck_pending_question = true;
                    schedule::Completion::Crashed(error.to_string())
                }
            });
            // Name the session once it has something to be named after.
            // Detached and unawaited: a title is never worth delaying a
            // prompt for, and a failure leaves the session as it was.
            if app.topic.is_none()
                && topic_handle.is_none()
                && matches!(completions.first(), Some(schedule::Completion::Root(Ok(_))))
            {
                let resolver = resolver.clone();
                let store = store.clone();
                let session_id = session_id.to_string();
                let system_prompt = system_prompt.to_string();
                topic_handle = Some(tokio::spawn(async move {
                    let model = store
                        .load(&session_id)
                        .map(|session| session.effective_model())
                        .unwrap_or_default();
                    let provider = resolver.resolve_provider(&model).ok()?;
                    ilar::topic::title_session(
                        provider.as_provider(),
                        &store,
                        &session_id,
                        Some(&system_prompt),
                    )
                    .await
                    .ok()
                    .flatten()
                }));
            }
        }
        engaged |= turn_handle.is_some();
        if let Some(handle) = adoption_handle.as_mut()
            && handle.is_finished()
        {
            match adoption_handle.take().unwrap().await {
                Ok(recovered) if !recovered.is_empty() => {
                    let count = recovered.len();
                    let (parcels, pause) = adopt_recovered(recovered);
                    held_notifications.extend(parcels);
                    if pause && !engaged {
                        notifications_paused = true;
                        // Persistent on purpose: it stands while the
                        // user reads, and the first turn's StartTurn
                        // clears it — the same gesture that resumes
                        // delivery.
                        app.set_persistent_notice(
                            format!(
                                "{count} task result(s) from a previous run held — send a message to deliver"
                            ),
                            NoticeLevel::Info,
                        );
                    }
                }
                Ok(_) => {}
                Err(join_error) => {
                    // The entries stay on disk for the next open;
                    // saying so beats silence.
                    app.set_notice(
                        format!("outbox adoption failed: {join_error}"),
                        NoticeLevel::Warning,
                    );
                }
            }
        }
        if let Some((_, handle)) = restore_handle.as_mut()
            && handle.is_finished()
        {
            let (restore_at, handle) = restore_handle.take().unwrap();
            match handle.await {
                Ok((view, priced)) => {
                    // The last turn died and nothing has happened
                    // since: the resume is still on offer, and a
                    // restored session used to lose it — the log says
                    // "error" and Ctrl-R did nothing.
                    let resume_offer = view.resume_offer;
                    // History goes where the open stood: after the
                    // banner, ahead of startup notices and anything
                    // else pushed while the worker ran.
                    app.land_restored_view(view, restore_at);
                    if resume_offer && turn_handle.is_none() && !app.retry_available {
                        app.retry_available = true;
                        app.set_persistent_notice(
                            "the last turn ended in an error — Ctrl-R resumes it",
                            NoticeLevel::Warning,
                        );
                    }
                    // A turn that ran meanwhile may have reported the
                    // exact context; the estimate must not regress it.
                    if app.context_estimated {
                        app.context_used = app.context_used.max(priced);
                    }
                }
                Err(join_error) => {
                    // The transcript stays whatever was drawn; saying
                    // so beats a spinner that never resolves.
                    let message = format!("session restore failed: {join_error}");
                    app.push_transcript_line(Line_::System(message.clone()));
                    app.set_notice(message, NoticeLevel::Error);
                }
            }
            // The restore held `busy` only for itself; a turn or a
            // question that started meanwhile keeps its own claim.
            if turn_handle.is_none() && app.question_modal.is_none() {
                app.busy = false;
                app.status = "ready".into();
                app.set_activity(Activity::Ready);
                if !app.queued_messages.is_empty() {
                    intents.push(Intent::SendQueued);
                }
            }
        }
        if let Some((_, handle)) = focus_seed.as_mut()
            && handle.is_finished()
        {
            let (for_session, handle) = focus_seed.take().unwrap();
            let seeded = handle.await.unwrap_or_else(|join_error| {
                Err(format!("cannot open agent transcript: {join_error}"))
            });
            land_agent_focus(app, &for_session, seeded);
        }
        if let Some((_, _, handle)) = rewind_task.as_mut()
            && handle.is_finished()
        {
            let (discarded, paused_before, handle) = rewind_task.take().unwrap();
            let outcome = match handle.await {
                Ok(result) => result,
                Err(join_error) => Err(anyhow::anyhow!("rewind task died: {join_error}")),
            };
            match outcome {
                Ok(report) => {
                    leave_session(
                        &spawner,
                        &mut aside_cancel,
                        &mut aside_handle,
                        &mut topic_handle,
                    )
                    .await;
                    let mut notice = if report.tree_restored {
                        format!("rewound {discarded} turn(s) · tree restored")
                    } else {
                        format!("rewound {discarded} turn(s) · no tree snapshot")
                    };
                    if report.head_moved {
                        notice.push_str(" · HEAD moved since (commits kept)");
                    }
                    // A message typed mid-rewind queued behind `busy`;
                    // the reopened session has no queue to inherit, so
                    // it rides the prefill instead of dying with the
                    // app. (Images attached to one do not survive the
                    // trip — a rewind lasts seconds; acceptable.)
                    let mut prefill = report.unsent;
                    for queued in app.queued_messages.drain(..) {
                        if !prefill.is_empty() {
                            prefill.push_str("\n\n");
                        }
                        prefill.push_str(&queued.text);
                    }
                    return Ok(AppExit::SwitchInto {
                        id: session_id.to_string(),
                        prefill: Some(prefill),
                        notice: Some(notice),
                        stash: std::mem::take(&mut app.input_stash),
                    });
                }
                Err(error) => {
                    notifications_paused = paused_before;
                    app.busy = false;
                    app.status = "ready".into();
                    app.set_activity(Activity::Ready);
                    app.set_notice(format!("rewind failed: {error}"), NoticeLevel::Error);
                }
            }
        }
        if let Some(handle) = topic_handle.as_mut()
            && handle.is_finished()
            && let Ok(topic) = topic_handle.take().unwrap().await
        {
            app.topic = topic;
            apply_terminal_title(app.topic.as_deref());
        }
        if let Some(handle) = aside_handle.as_mut()
            && handle.is_finished()
        {
            aside_cancel = None;
            if let Ok((question, result)) = aside_handle.take().unwrap().await {
                app.finish_aside(question, result);
            }
        }
        // Finished deliveries join here; a crashed delivery task is
        // folded as a failure so the notification it carried is
        // salvaged rather than lost with the panic.
        let mut index = 0;
        while index < routed.len() {
            if !routed[index].handle.is_finished() {
                index += 1;
                continue;
            }
            let delivery = routed.remove(index);
            let result = match delivery.handle.await {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("the delivery task crashed: {error}")),
            };
            completions.push(schedule::Completion::Routed {
                result,
                // Asked before the parcel moves: the token is the only
                // witness that this requeue was a cancel, not a busy
                // target.
                cancelled: delivery.cancel.is_cancelled(),
                parcel: delivery.parcel,
            });
        }

        // A focus message's ending: a running agent took it (or queued
        // it), a finished one answered, or the send failed. Said in the
        // root's transcript, where the send was recorded.
        let mut index = 0;
        while index < focus_messages.len() {
            if !focus_messages[index].handle.is_finished() {
                index += 1;
                continue;
            }
            let message = focus_messages.remove(index);
            let outcome = match message.handle.await {
                Ok(outcome) => outcome,
                Err(error) => ilar::subagent::TaskMessage::Refused(format!(
                    "the message task crashed: {error}"
                )),
            };
            let (line, level) = focus_outcome_line(&message.target, outcome);
            // The transcript always keeps it; the notice line is for
            // the ones that need answering.
            if level != NoticeLevel::Info {
                app.set_notice(&line, level);
            }
            app.push_transcript_line(Line_::System(line));
        }

        // The whole iteration minus the dispatch — completion
        // bookkeeping and its after_turn decisions, the intent drain,
        // the palette peek, the notification gate, the subtask spawn,
        // the frame and the poll — lives in schedule::tick so tests
        // can drive the sequence. Only the effects live here.
        let outcome = schedule::tick(
            app,
            completions,
            std::mem::take(&mut intents),
            &mut LoopRuntime {
                turn_handle: &mut turn_handle,
                aside_handle: &mut aside_handle,
                aside_cancel: &mut aside_cancel,
                events_rx: &mut events_rx,
                cancel: &mut cancel,
                steer_tx: &mut steer_tx,
                pending_terminal_event: &mut pending_terminal_event,
                held_notifications: &mut held_notifications,
                notifications: &mut notifications,
                routed: &mut routed,
                session_labels: &mut session_labels,
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
                outbox_dir: &outbox_dir,
                terminal: &mut *terminal,
                bell_pending: &mut bell_pending,
            },
        )
        .await?;
        let event = match outcome {
            schedule::Tick::Idle => continue,
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
                        // Focus counts as something open: Ctrl-C rides
                        // the Esc path and closes the view instead of
                        // shrugging "nothing to interrupt".
                        app.has_modal() || app.model_key_pending || app.focus.is_some(),
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
                // The exit, EOF-style: a blank prompt with nothing open
                // — and a focus view is something open.
                let quitting = quit_requested(
                    code,
                    control,
                    app.has_modal() || app.focus.is_some(),
                    app.input.is_blank(),
                );
                // Any other key ends a pending quit confirmation, so the
                // warning always describes the keypress before it. The
                // focus view's cancel arms the same way and must expire
                // the same way — here, not inside the focus branch,
                // which several keys (Ctrl-L, an armed quit) never
                // reach.
                if !quitting {
                    app.quit_armed = false;
                }
                if !matches!((code, control), (KeyCode::Char('g'), true)) {
                    app.disarm_focus_cancel();
                }
                if quitting {
                    // Quitting under a running rewind would kill git
                    // mid-restore and leave half a working tree. It
                    // finishes in seconds; the quit can wait for it.
                    if rewind_task.is_some() {
                        app.set_notice("rewinding — quit after it finishes", NoticeLevel::Warning);
                        continue;
                    }
                    // Everything the exit takes down is invisible from a
                    // blank prompt; say what leaving costs before the
                    // second press takes it. Task results survive in
                    // the outbox — the warning says when they arrive,
                    // not that they are lost. Only the loop's own
                    // counts are handed over: the turn, the goal, the
                    // stash and every waiting message are the app's,
                    // and it counts those itself.
                    let cost = crate::app::QuitCost {
                        undelivered: held_notifications.len() + notifications.len() + routed.len(),
                        // `spawner.shutdown()` below cancels every one
                        // of these, and each cancelled task mails a
                        // "was cancelled" result to its parent. The
                        // deliveries are not counted twice: they are
                        // cancelled too, and what that costs — mail
                        // that waits for the next open — is exactly
                        // what the undelivered line already says.
                        background: spawner.running_background(),
                        focus_messages: focus_messages.len(),
                    };
                    if let Some(warning) = app.quit_warning(cost) {
                        // Over anything standing: a second Ctrl-D quits.
                        app.set_notice_now(warning, NoticeLevel::Warning);
                        continue;
                    }
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                    for delivery in &routed {
                        delivery.cancel.cancel();
                    }
                    for message in &focus_messages {
                        message.handle.abort();
                    }
                    let _ =
                        futures::future::join_all(routed.drain(..).map(|delivery| delivery.handle))
                            .await;
                    spawner.shutdown().await;
                    return Ok(AppExit::Quit);
                }
                // A pure repaint, so it runs before the modal dispatch
                // rather than behind it: outside damage is likeliest
                // while an overlay is up, and the user should not have
                // to dismiss the overlay to clear the screen. It touches
                // nothing but the next frame.
                if matches!((code, control), (KeyCode::Char('l'), true)) {
                    app.force_full_redraw = true;
                    continue;
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
                                    spawn_root_turn(
                                        app,
                                        TurnSlots {
                                            handle: &mut turn_handle,
                                            events_rx: &mut events_rx,
                                            cancel: &mut cancel,
                                            steer_tx: &mut steer_tx,
                                            ring_on_completion: &mut ring_on_turn_completion,
                                        },
                                        TurnDeps {
                                            resolver: &resolver,
                                            store,
                                            session_id,
                                            system_prompt,
                                            registry,
                                            tool_ctx: &tool_ctx,
                                            loop_config: &loop_config,
                                        },
                                        RootTurn::Answer(response),
                                        Bell::Ring,
                                    );
                                }
                            }
                        }
                        Modal::Password => {
                            let modal = app.password_modal.as_mut().expect("password modal");
                            if let PasswordAction::Answer(answer) = modal.handle_key(key) {
                                let line = modal.outcome_line(answer.is_some());
                                app.password_modal = None;
                                deliver_answer(
                                    app,
                                    &mut password_reply,
                                    answer,
                                    line,
                                    "running sudo",
                                );
                            }
                        }
                        Modal::Grant => {
                            let modal = app.grant_modal.as_mut().expect("grant modal");
                            if let GrantAction::Answer(answer) = modal.handle_key(key) {
                                let line = modal.outcome_line(answer);
                                app.grant_modal = None;
                                deliver_answer(
                                    app,
                                    &mut grant_reply,
                                    answer,
                                    line,
                                    "processing grant",
                                );
                            }
                        }
                        Modal::PendingManager => match app.pending_manager_key(code, control) {
                            PendingAction::Stay => {}
                            PendingAction::Close => app.pending_manager = None,
                            PendingAction::DeleteQueued(index) => {
                                if index < app.queued_messages.len() {
                                    let removed = app.queued_messages.remove(index);
                                    // The images go with it: they were
                                    // attached to this message, not to
                                    // the prompt it never reached.
                                    app.set_notice(
                                        format!(
                                            "removed queued message: {}",
                                            crate::transcript::pending_summary(&removed)
                                        ),
                                        NoticeLevel::Info,
                                    );
                                }
                            }
                            PendingAction::EditQueued(index) => {
                                if index < app.queued_messages.len() {
                                    let message = app.queued_messages.remove(index);
                                    // Pulled back whole: the text into
                                    // the prompt, the attachments back
                                    // onto it, so re-sending sends the
                                    // same message.
                                    app.input = InputBuffer::from(message.text);
                                    app.pending_images.splice(0..0, message.images);
                                    app.pending_manager = None;
                                }
                            }
                            PendingAction::AbortGoal => {
                                if let Some(message) = app.abort_goal() {
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
                                // Deliveries are background work too: a
                                // cancelled one requeues its
                                // notification and waits for resume.
                                for delivery in &routed {
                                    delivery.cancel.cancel();
                                }
                                notifications_paused = true;
                                app.background_running = 0;
                                app.set_persistent_notice(
                                "background tasks cancelled; task results held — send a message to deliver",
                                NoticeLevel::Warning,
                            );
                            }
                            PendingAction::StopServices => {
                                services.stop_all();
                                app.services_running = 0;
                                app.set_notice("services stopped", NoticeLevel::Info);
                            }
                            PendingAction::DeliverHeld => {
                                // Lifting the pause is the whole
                                // mechanism: the gate starts the
                                // delivery turn on the next idle pass,
                                // in the order the backlog holds.
                                notifications_paused = false;
                                // The standing "held" reminder is what
                                // this press answers, and a transient
                                // notice may not bury a standing one.
                                app.clear_notice();
                                app.set_notice(
                                    format!(
                                        "delivering {} held task result(s)",
                                        app.held_results.len()
                                    ),
                                    NoticeLevel::Info,
                                );
                                app.pending_manager = None;
                            }
                            PendingAction::DismissRetry => {
                                app.retry_available = false;
                                app.clear_transient_notice();
                            }
                            PendingAction::RetryNow => {
                                let state = observe(
                                    app,
                                    &turn_handle,
                                    &pending_terminal_event,
                                    &steer_tx,
                                    notifications_paused,
                                );
                                let decided = retry_intents(&state, app.busy);
                                if retry_dismisses_manager(&decided) {
                                    app.pending_manager = None;
                                }
                                intents.extend(decided);
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
                        Modal::Aside => match (code, control) {
                            (KeyCode::Up, _) => {
                                let aside = app.aside.as_mut().unwrap();
                                aside.scroll = aside.scroll.saturating_sub(1);
                            }
                            (KeyCode::Down, _) => {
                                let aside = app.aside.as_mut().unwrap();
                                aside.scroll = aside.scroll.saturating_add(1);
                            }
                            (KeyCode::PageUp, _) => {
                                let aside = app.aside.as_mut().unwrap();
                                aside.scroll = aside.scroll.saturating_sub(10);
                            }
                            (KeyCode::PageDown, _) => {
                                let aside = app.aside.as_mut().unwrap();
                                aside.scroll = aside.scroll.saturating_add(10);
                            }
                            (KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter, false) => {
                                app.aside = None;
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
                        Modal::ContextPicker => {
                            let action = {
                                let picker = app.context_picker.as_mut().unwrap();
                                picker.handle_key(code, control)
                            };
                            apply_context_picker_action(app, action);
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
                                SessionPickerAction::Delete(id) => {
                                    // Read before the delete: afterwards
                                    // there is no head to name it by.
                                    let name = session_name(store, &id);
                                    match store.delete(&id) {
                                        Ok(()) => {
                                            if let Some(picker) = app.session_picker.as_mut() {
                                                picker.sessions.retain(|session| session.id != id);
                                                // Through the hook, not the field:
                                                // select() also disarms.
                                                picker.select(0);
                                            }
                                            app.set_notice(
                                                format!("deleted session {name}"),
                                                NoticeLevel::Info,
                                            );
                                        }
                                        Err(error) => {
                                            app.set_notice(
                                                format!("cannot delete {name}: {error}"),
                                                NoticeLevel::Error,
                                            );
                                        }
                                    }
                                }
                                SessionPickerAction::Fork(id) => {
                                    let blocked = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        routed.len(),
                                        !app.input.is_blank(),
                                        app.goal.is_some(),
                                    );
                                    if let Some(reason) = blocked {
                                        app.set_notice(reason, NoticeLevel::Warning);
                                        continue;
                                    }
                                    match store.fork(&id) {
                                        Ok(fork_id) => {
                                            leave_session(
                                                &spawner,
                                                &mut aside_cancel,
                                                &mut aside_handle,
                                                &mut topic_handle,
                                            )
                                            .await;
                                            return Ok(AppExit::SwitchInto {
                                                id: fork_id,
                                                prefill: None,
                                                notice: None,
                                                stash: std::mem::take(&mut app.input_stash),
                                            });
                                        }
                                        Err(error) => {
                                            app.set_notice(
                                                format!(
                                                    "cannot fork {}: {error}",
                                                    session_name(store, &id)
                                                ),
                                                NoticeLevel::Error,
                                            );
                                        }
                                    }
                                }
                                SessionPickerAction::Resume(new_session) => {
                                    let blocked = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        routed.len(),
                                        !app.input.is_blank(),
                                        app.goal.is_some(),
                                    );
                                    if let Some(reason) = blocked {
                                        app.set_notice(reason, NoticeLevel::Warning);
                                        continue;
                                    }
                                    match direct_resume_blocked(store, &new_session) {
                                        Some(reason) => app.set_notice(reason, NoticeLevel::Error),
                                        None => {
                                            leave_session(
                                                &spawner,
                                                &mut aside_cancel,
                                                &mut aside_handle,
                                                &mut topic_handle,
                                            )
                                            .await;
                                            return Ok(AppExit::SwitchInto {
                                                id: new_session,
                                                prefill: None,
                                                notice: None,
                                                stash: std::mem::take(&mut app.input_stash),
                                            });
                                        }
                                    }
                                }
                                SessionPickerAction::ContentSearch => {
                                    app.session_picker = None;
                                    app.session_search = Some(SessionSearch::new());
                                }
                            }
                        }
                        Modal::SessionSearch => {
                            let action = app
                                .session_search
                                .as_mut()
                                .unwrap()
                                .handle_key(code, control);
                            match action {
                                SessionSearchAction::Stay => {}
                                SessionSearchAction::Rescan => {
                                    stop_session_scan(&mut search_cancel, &mut search_rx);
                                }
                                SessionSearchAction::Dismiss => {
                                    stop_session_scan(&mut search_cancel, &mut search_rx);
                                    app.session_search = None;
                                    app.clear_transient_notice();
                                }
                                SessionSearchAction::ListMode => {
                                    stop_session_scan(&mut search_cancel, &mut search_rx);
                                    app.session_search = None;
                                    let sessions = store
                                        .list()
                                        .into_iter()
                                        .filter(|session| session.id != app.session_id)
                                        .collect();
                                    app.session_picker = Some(SessionPicker::new(
                                        sessions,
                                        Some(tool_ctx.location.cwd().to_path_buf()),
                                    ));
                                }
                                SessionSearchAction::Resume(new_session) => {
                                    // This modal covers the notice line,
                                    // so every refusal it can raise is
                                    // handed to the modal to draw.
                                    let blocked = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        routed.len(),
                                        !app.input.is_blank(),
                                        app.goal.is_some(),
                                    )
                                    .or_else(|| direct_resume_blocked(store, &new_session));
                                    if let Some(reason) = blocked {
                                        if let Some(search) = app.session_search.as_mut() {
                                            search.refusal = Some(reason);
                                        }
                                        continue;
                                    }
                                    stop_session_scan(&mut search_cancel, &mut search_rx);
                                    leave_session(
                                        &spawner,
                                        &mut aside_cancel,
                                        &mut aside_handle,
                                        &mut topic_handle,
                                    )
                                    .await;
                                    return Ok(AppExit::SwitchInto {
                                        id: new_session,
                                        prefill: None,
                                        notice: None,
                                        stash: std::mem::take(&mut app.input_stash),
                                    });
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
                                        routed.len(),
                                        false,
                                        app.goal.is_some(),
                                    ) {
                                        app.set_notice(reason, NoticeLevel::Warning);
                                        continue;
                                    }
                                    app.turn_picker = None;
                                    // Held: a notification turn starting
                                    // mid-rewind would write the log the
                                    // rewind is rewriting. The prior pause
                                    // state comes back on failure; success
                                    // reopens the session.
                                    let paused_before = notifications_paused;
                                    notifications_paused = true;
                                    app.busy = true;
                                    app.status = "rewinding…".into();
                                    app.set_activity(Activity::Thinking);
                                    let store = store.clone();
                                    let rewind_session_id = session_id.to_string();
                                    let cwd = tool_ctx.cwd.clone();
                                    rewind_task = Some((
                                        discarded,
                                        paused_before,
                                        tokio::spawn(async move {
                                            ilar::rewind::rewind_session(
                                                &store,
                                                &rewind_session_id,
                                                cut,
                                                &target,
                                                &cwd,
                                            )
                                            .await
                                        }),
                                    ));
                                }
                                TurnPickerAction::Fork { cut, target } => {
                                    if let Some(reason) = switch_blocked(
                                        turn_handle.is_some(),
                                        spawner.running_background(),
                                        routed.len(),
                                        false,
                                        app.goal.is_some(),
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
                                            leave_session(
                                                &spawner,
                                                &mut aside_cancel,
                                                &mut aside_handle,
                                                &mut topic_handle,
                                            )
                                            .await;
                                            return Ok(AppExit::SwitchInto {
                                                id: fork_id,
                                                prefill: unsent,
                                                notice: Some(format!(
                                                    "forked at that turn from {}",
                                                    session_name(store, session_id)
                                                )),
                                                stash: std::mem::take(&mut app.input_stash),
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
                                        app.model_picker = None;
                                        app.variant_picker = Some(VariantPicker::new(
                                            model,
                                            &app.current_model,
                                            app.current_variant.as_deref(),
                                        ));
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
                                }
                            }
                        }
                    }
                    continue;
                }
                // A focus view in front owns the keyboard the way a
                // modal does: scroll keys move it, Esc closes it — and
                // must not fall through to the turn-abort arm below,
                // because closing a view is not aborting the root's
                // work. Enter talks to the agent on screen; the root's
                // own chords are not routed here and say so.
                if app.focus.is_some() {
                    // Ctrl-G stops the agent on screen — the one cancel
                    // that is not all-or-nothing. It arms first: this
                    // throws work away, and the panel's only other
                    // cancel (`d d` in Ctrl-Q) confirms too. Not Ctrl-X,
                    // which is already the model/theme prefix: a user
                    // reaching for models would arm a cancel and then
                    // confirm it out of habit.
                    if matches!((code, control), (KeyCode::Char('g'), true)) {
                        match app.focus_cancel_key() {
                            Some((_, FocusCancel::Armed)) => app.set_notice(
                                "press Ctrl-G again to cancel this agent",
                                NoticeLevel::Warning,
                            ),
                            Some((session_id, FocusCancel::Fire)) => {
                                if spawner.cancel_task(&session_id) {
                                    // Held, not delivered: the user is
                                    // at the keyboard, exactly as after
                                    // an abort or a cancel-all.
                                    notifications_paused = true;
                                    app.set_notice(
                                        "cancelling this agent — its result is held until your next message",
                                        NoticeLevel::Warning,
                                    );
                                } else {
                                    app.set_notice(
                                        "nothing to cancel — a finished agent has already stopped, and one running inside the turn goes with Esc",
                                        NoticeLevel::Info,
                                    );
                                }
                            }
                            None => {}
                        }
                        continue;
                    }
                    if code == KeyCode::Esc {
                        app.close_focus();
                        continue;
                    }
                    if let Some(named) = crate::app::focus_key_belongs_to_the_root(
                        code,
                        control,
                        app.input.is_blank(),
                    ) {
                        app.set_notice(
                            format!("{named} belongs to the session behind this view — Esc leaves the view first"),
                            NoticeLevel::Info,
                        );
                        continue;
                    }
                    let scrolled = app.focus.as_mut().is_some_and(|focus| {
                        match code {
                            KeyCode::Up => focus.scroll_by(-1),
                            KeyCode::Down => focus.scroll_by(1),
                            KeyCode::PageUp => focus.scroll_by(-(focus.page_size() as isize)),
                            KeyCode::PageDown => focus.scroll_by(focus.page_size() as isize),
                            KeyCode::Home => focus.scroll_to_top(),
                            KeyCode::End => focus.scroll_to_tail(),
                            _ => return false,
                        }
                        true
                    });
                    if scrolled {
                        continue;
                    }
                    // Everything else is the prompt's: typing talks to
                    // the agent on screen, and Enter sends it the way
                    // the model's task_message would — steering it if
                    // it runs, resuming it if it finished.
                    match handle_prompt_key(&mut app.input, key) {
                        PromptAction::Submit if !app.input.is_blank() => {
                            let focus = app.focus.as_ref().expect("focus is open");
                            let (session_id, target) =
                                (focus.session_id.clone(), focus.title.clone());
                            // Refused before anything is recorded: the
                            // send used to reach the transcript first
                            // and the model's refusal — with a raw uuid
                            // in it — a moment later.
                            if let Some(refusal) = crate::app::focus_send_refusal(focus) {
                                app.set_notice(refusal, NoticeLevel::Warning);
                                continue;
                            }
                            // A slash command is the root's, and a
                            // focus view is not where it runs; sending
                            // the literal `/sessions` to an agent is
                            // never what was meant.
                            if app.input.text().trim_start().starts_with('/') {
                                app.set_notice(
                                    "commands belong to the session behind this view — Esc leaves the view first",
                                    NoticeLevel::Warning,
                                );
                                continue;
                            }
                            let text = app.input.take();
                            app.history.push(&text);
                            app.push_transcript_line(Line_::System(focus_message_line(
                                &target, &text,
                            )));
                            app.set_notice(format!("sending to {target}…"), NoticeLevel::Info);
                            let spawner = spawner.clone();
                            let ctx = tool_ctx.clone();
                            let message = text;
                            let handle = tokio::spawn(async move {
                                spawner
                                    .deliver_to_task(
                                        ilar::subagent::TaskMessageInput {
                                            task_id: session_id,
                                            message,
                                            workspace: None,
                                        },
                                        &ctx,
                                    )
                                    .await
                            });
                            focus_messages.push(FocusMessage { handle, target });
                        }
                        PromptAction::Edited => app.clear_transient_notice(),
                        PromptAction::Unhandled | PromptAction::Submit => {}
                    }
                    continue;
                }
                // The session on offer answers the keyboard before the
                // ordinary handling does: Enter on an empty prompt
                // resumes it, and anything typed leaves it behind —
                // the keystroke itself falls through and lands in the
                // prompt either way. Everything else leaves the offer
                // standing, including the Esc that clears a draft,
                // which has its own work to do.
                if app.ghost.is_some() {
                    let state = observe(
                        app,
                        &turn_handle,
                        &pending_terminal_event,
                        &steer_tx,
                        notifications_paused,
                    );
                    match decide::ghost_step(&state, key) {
                        decide::GhostStep::Resume => {
                            // The picker's resume, to the letter: the
                            // same refusals, the same handover, the
                            // same restart. A session offered must not
                            // open by a path the picker does not use.
                            let id = app
                                .ghost
                                .as_ref()
                                .expect("an offer is up")
                                .session_id
                                .clone();
                            if let Some(reason) = switch_blocked(
                                turn_handle.is_some(),
                                spawner.running_background(),
                                routed.len(),
                                !app.input.is_blank(),
                                app.goal.is_some(),
                            )
                            .or_else(|| direct_resume_blocked(store, &id))
                            {
                                app.set_notice(reason, NoticeLevel::Warning);
                                continue;
                            }
                            leave_session(
                                &spawner,
                                &mut aside_cancel,
                                &mut aside_handle,
                                &mut topic_handle,
                            )
                            .await;
                            return Ok(AppExit::SwitchInto {
                                id,
                                prefill: None,
                                notice: None,
                                stash: std::mem::take(&mut app.input_stash),
                            });
                        }
                        decide::GhostStep::Dismiss => {
                            app.dismiss_ghost();
                        }
                        decide::GhostStep::Keep => {}
                    }
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
                    if matches!(code, KeyCode::Char('m' | 'M')) {
                        // The leader's one refusal, said by name — the
                        // same rule the palette follows.
                        if app.busy || model_choices.is_empty() {
                            app.set_notice(
                                if model_choices.is_empty() {
                                    "no models to switch between — see docs/configuration.md"
                                } else {
                                    "a turn is running — switch models between turns"
                                },
                                NoticeLevel::Info,
                            );
                            continue;
                        }
                        app.clear_transient_notice();
                        app.model_picker =
                            Some(ModelPicker::new(model_choices.clone(), &app.current_model));
                        continue;
                    }
                    // Like F3: paint, so a running turn is no reason to
                    // refuse it.
                    if matches!(code, KeyCode::Char('t' | 'T')) {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                        continue;
                    }
                    app.status = "ready".into();
                    app.clear_transient_notice();
                }
                // Armed mid-turn too: its T half is only paint, and its
                // M half refuses above by name rather than under a dead
                // prefix.
                if matches!((code, control), (KeyCode::Char('x'), true)) {
                    app.model_key_pending = true;
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
                    // Still answers: a documented shortcut that does
                    // nothing at all reads as a broken keyboard.
                    (KeyCode::Char('m'), true) | (KeyCode::F(2), false) => {
                        app.set_notice(
                            if model_choices.is_empty() {
                                "no models to switch between — see docs/configuration.md"
                            } else {
                                "a turn is running — F2 picks a model between turns"
                            },
                            NoticeLevel::Info,
                        );
                    }
                    // No busy guard: a theme is paint, and repainting
                    // mid-turn costs the turn nothing.
                    (KeyCode::F(3), false) => {
                        app.clear_transient_notice();
                        app.theme_picker = Some(ThemePicker::new(app.theme));
                    }
                    (KeyCode::Esc, _) => {
                        // Esc is strictly immediate-scope: abort the running
                        // turn or clear the input. Standing state (goal,
                        // queue, background jobs) lives in the pending
                        // manager (Ctrl-Q) and explicit commands. Ctrl-C
                        // arrives here too — it is rewritten into Esc above.
                        if rewind_task.is_some() {
                            // Git is mid-restore; stopping it would leave
                            // half a working tree. Say so instead of
                            // silently doing nothing.
                            app.set_notice(
                                "a rewind cannot be aborted — it finishes in seconds",
                                NoticeLevel::Warning,
                            );
                        } else if let Some(cancel) = cancel.as_ref().filter(|_| app.busy) {
                            cancel.cancel();
                            app.status = "aborting…".into();
                            let detached = app
                                .background_running
                                .saturating_sub(app.deliveries_in_flight);
                            // Cancel-all pauses notifications for
                            // exactly this reason: the dying children's
                            // completions would otherwise start a fresh
                            // turn nobody asked for, moments after the
                            // user said stop. Only when something is
                            // actually detached, though — a pause with
                            // nothing to hold is an unexplained one,
                            // and it would sit on every other session's
                            // mail until the next completed turn.
                            if detached > 0 {
                                notifications_paused = true;
                            }
                            app.set_notice(abort_notice(detached), NoticeLevel::Warning);
                            app.set_activity(Activity::Aborting);
                        } else {
                            // Busy with nothing to cancel — the restore
                            // holds `busy` for itself — still answers:
                            // the draft is the user's either way, and a
                            // dead Esc while "restoring session" reads
                            // as a hung app.
                            let stashed = app.discard_or_stash_input();
                            match (stashed, app.busy) {
                                (Some(notice), _) => app.set_notice(notice, NoticeLevel::Info),
                                (None, true) => app.set_notice(
                                    "nothing to abort yet — the session is still loading",
                                    NoticeLevel::Info,
                                ),
                                (None, false) => {}
                            }
                        }
                    }
                    // Ctrl-V attaches a clipboard *image*; text arrives
                    // as an ordinary terminal paste event regardless.
                    (KeyCode::Char('v'), true) => match app.read_clipboard_image() {
                        Ok(Some(image)) => {
                            app.attach_image(image);
                        }
                        Ok(None) => app.set_notice(
                            "no image on the clipboard (text pastes normally)",
                            NoticeLevel::Info,
                        ),
                        Err(error) => {
                            app.set_notice(format!("clipboard: {error:#}"), NoticeLevel::Error);
                        }
                    },
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
                    (KeyCode::Char('s'), true) => {
                        app.stash_or_pop_input();
                    }
                    // Ctrl-L is claimed above the modal dispatch.
                    (KeyCode::Char('o'), true) => {
                        app.open_link_picker();
                    }
                    // Read-only, like the link picker: no busy guard.
                    (KeyCode::Char('t'), true) => {
                        app.todos_visible = true;
                        app.todos_scroll = 0;
                    }
                    // Unconditional: `retry_intents` answers a Ctrl-R
                    // there is nothing to resume from, which was a
                    // silent keypress before.
                    (code, control) if retry_requested(code, control) => {
                        let state = observe(
                            app,
                            &turn_handle,
                            &pending_terminal_event,
                            &steer_tx,
                            notifications_paused,
                        );
                        intents.extend(retry_intents(&state, app.busy));
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
                            // Attached images are not part of the
                            // decision: `apply_intent` takes them off
                            // the prompt whichever way the message goes.
                            let decided = decide::submit(&state, app.busy, text.clone());
                            // A refused command keeps its text, exactly
                            // as `prepare_prompt`'s refusals do: the
                            // user retypes nothing to fix a typo or to
                            // wait for the turn to end.
                            if decide::refused(&decided) {
                                app.input = InputBuffer::from(text.as_str());
                            }
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
                // A paste into the content search invalidates the scan
                // in flight, exactly as a keystroke does; the generation
                // moving is how the modal says the query changed. Only
                // the cancellation happens here — the spawner below the
                // drain starts the new scan.
                let scan = app.session_search.as_ref().map(|search| search.generation);
                let decided = decide::paste(&state, text);
                apply_event_intents(app, decided, &mut intents, steer_tx.as_ref());
                // A paste into the prompt is typing: the draft has
                // started, so the offer above it is answered. A paste
                // into a modal's field leaves the prompt blank, and one
                // into a focus view's prompt is addressed to that agent
                // — neither is a message to the fresh session, so both
                // leave the offer alone, exactly as typing there does.
                if app.ghost.is_some() && app.focus.is_none() && !app.input.is_blank() {
                    app.dismiss_ghost();
                }
                if scan != app.session_search.as_ref().map(|search| search.generation) {
                    stop_session_scan(&mut search_cancel, &mut search_rx);
                }
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
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {
                let batch = drain_motion_batch(
                    (mouse.column, mouse.row),
                    MAX_WHEEL_EVENTS_PER_BATCH,
                    || {
                        if crossterm::event::poll(std::time::Duration::ZERO)? {
                            Ok(Some(crossterm::event::read()?))
                        } else {
                            Ok(None)
                        }
                    },
                )?;
                pending_terminal_event = batch.deferred;
                app.update_hover(batch.column, batch.row);
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
                        // Sidebar chrome first: the disclosure rows and
                        // the agents map are the clickables outside the
                        // transcript. While a focus view is up the
                        // transcript under it must not take selection
                        // clicks meant for rows it is not showing.
                        if !app.click_exited_services(mouse.column, mouse.row)
                            && !app.click_agents_more(mouse.column, mouse.row)
                        {
                            match app.click_agent_row(mouse.column, mouse.row) {
                                Some(AgentTarget::Main) => {
                                    app.close_focus();
                                }
                                Some(AgentTarget::Focus(id)) => {
                                    // `None`: the view was already on
                                    // that agent, and re-seeding it
                                    // would replace a live tail with a
                                    // settled replay.
                                    if let Some(streaming) = open_agent_focus(app, store, &id) {
                                        let store = store.clone();
                                        let seed_id = id.clone();
                                        focus_seed = Some((
                                            id,
                                            tokio::task::spawn_blocking(move || {
                                                seed_agent_focus(&store, &seed_id, streaming)
                                            }),
                                        ));
                                    }
                                }
                                None if app.focus.is_none() => {
                                    app.begin_transcript_selection(mouse.column, mouse.row);
                                }
                                None => {}
                            }
                        }
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
    #[test]
    fn a_focus_message_is_recorded_as_sent_to_its_target() {
        assert_eq!(
            super::focus_message_line("explorer · survey the API", "check the auth module too"),
            "→ explorer · survey the API: check the auth module too"
        );
    }

    /// What the root's transcript says about a focus message, in the
    /// TUI's own words: the tool's text is written for a model, and a
    /// resumed agent's whole reply — up to 16 KiB of unrendered
    /// markdown — is not a record of anything, since the focus view
    /// already shows it rendered.
    #[test]
    fn a_focus_messages_ending_is_said_in_the_tuis_own_words() {
        use super::{NoticeLevel, focus_outcome_line};
        use ilar::subagent::TaskMessage;

        let target = "explorer · survey the API";
        let task_id = || "7c1e-0000".to_string();
        assert_eq!(
            focus_outcome_line(target, TaskMessage::Queued { task_id: task_id() }),
            (
                "explorer · survey the API takes it at its next step".into(),
                NoticeLevel::Info
            )
        );
        let (held, level) = focus_outcome_line(target, TaskMessage::Held { task_id: task_id() });
        assert!(held.contains("at its next resume"), "{held}");
        assert_eq!(level, NoticeLevel::Info);
        // No uuid and no "do not repeat the message" prose anywhere.
        let answer = format!(
            "The auth module is fine.\n\nDetails follow.\n{}\n\n(task_id: {})",
            "x".repeat(20_000),
            task_id()
        );
        let (answered, level) = focus_outcome_line(
            target,
            TaskMessage::Answered {
                task_id: task_id(),
                output: ilar::tools::ToolOutput::text(answer),
                still_queued: false,
            },
        );
        assert_eq!(
            answered,
            "explorer · survey the API answered: The auth module is fine."
        );
        assert_eq!(level, NoticeLevel::Info);
        let (failed, level) = focus_outcome_line(
            target,
            TaskMessage::Answered {
                task_id: task_id(),
                output: ilar::tools::ToolOutput::error(format!(
                    "the worktree is gone\n{}",
                    "x".repeat(20_000)
                )),
                still_queued: false,
            },
        );
        assert_eq!(
            failed,
            "message to explorer · survey the API failed: the worktree is gone"
        );
        assert_eq!(level, NoticeLevel::Error);
        // A resume that declined while the message stays parked is a
        // failure to report, not a message to retype.
        let (queued, level) = focus_outcome_line(
            target,
            TaskMessage::Answered {
                task_id: task_id(),
                output: ilar::tools::ToolOutput::error("that checkout is busy"),
                still_queued: true,
            },
        );
        assert_eq!(
            queued,
            "explorer · survey the API could not be resumed (that checkout is busy) — the \
             message waits for its next resume"
        );
        assert_eq!(level, NoticeLevel::Warning);
        let (refused, level) =
            focus_outcome_line(target, TaskMessage::Refused("nobody is listening".into()));
        assert!(
            refused.contains("was refused: nobody is listening"),
            "{refused}"
        );
        assert_eq!(level, NoticeLevel::Error);
        // An empty answer still says something.
        let (empty, _) = focus_outcome_line(
            target,
            TaskMessage::Answered {
                task_id: task_id(),
                output: ilar::tools::ToolOutput::text(format!("\n\n(task_id: {})", task_id())),
                still_queued: false,
            },
        );
        assert!(empty.ends_with("answered: (nothing)"), "{empty}");
        // Every one of them is one line: nothing here carries 16 KiB.
        for line in [answered, failed, queued, refused, empty] {
            assert_eq!(line.lines().count(), 1, "{line}");
            assert!(line.chars().count() < 300, "{line}");
        }
    }

    /// Every place that names a session for a delivery goes through
    /// one resolver: own session, roster row, log head, then the id.
    #[test]
    fn a_session_is_named_before_it_is_numbered() {
        use ilar::session::{SessionEvent, SessionMeta, SessionStore, new_id};
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let mut app = App::new();
        let mut cache = std::collections::HashMap::new();
        let own = new_id();
        let label = |app: &App, cache: &mut std::collections::HashMap<String, String>, id: &str| {
            super::session_label(app, &store, &own, cache, id)
        };

        assert_eq!(label(&app, &mut cache, &own), "this session");

        let running = new_id();
        app.agents_view.push(AgentRow {
            foreign_parent: None,
            session_id: running.clone(),
            depth: 0,
            description: "survey the API".into(),
            agent: "explorer".into(),
            background: true,
            delivering: false,
            elapsed: std::time::Duration::ZERO,
            waiting: false,
            quiet: None,
        });
        assert_eq!(
            label(&app, &mut cache, &running),
            "explorer · survey the API"
        );

        let finished = new_id();
        store
            .create(SessionMeta {
                session_id: finished.clone(),
                parent_id: Some(own.clone()),
                agent: "reviewer".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        let mut session = store.acquire_writer(&finished).unwrap().load().unwrap();
        session
            .append(SessionEvent::UserMessage {
                id: new_id(),
                text: format!("Review the diff for {}.", "x".repeat(80)),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);
        let named = label(&app, &mut cache, &finished);
        assert!(
            named.starts_with("reviewer · Review the diff for xxx"),
            "{named}"
        );
        assert!(named.ends_with('…'), "{named}");
        assert!(named.chars().count() < 70, "{named}");
        assert_eq!(cache.get(&finished), Some(&named), "remembered");

        let unknown = new_id();
        let fallback = label(&app, &mut cache, &unknown);
        assert_eq!(fallback, format!("session {}", short_session_id(&unknown)));
    }

    use super::*;
    use ilar::runtime::{create_root_session, restored_todos};
    use ilar::session::{SessionMeta, new_id};

    /// A login that stores tokens nothing points at is half a next
    /// step: the following `ilar` runs the default zai model and dies
    /// naming a key the person never had.
    #[test]
    fn login_ends_with_the_lines_that_make_the_account_usable() {
        let hint = chatgpt_setup_hint(std::path::Path::new("/home/x/.config/ilar/ilar.toml"));
        assert!(hint.contains("/home/x/.config/ilar/ilar.toml"), "{hint}");
        assert!(hint.contains("[providers.openai]"), "{hint}");
        assert!(hint.contains("auth = \"chatgpt\""), "{hint}");
        assert!(hint.contains("[general]"), "{hint}");
        assert!(
            hint.contains(ilar::model::CHATGPT_SUGGESTED_MODEL),
            "{hint}"
        );
    }

    /// A project file that exists but was refused is reported, after
    /// the config warnings and in the same transcript channel: a launch
    /// that quietly ignores the project's instructions is unreadable
    /// from inside the session.
    #[test]
    fn a_refused_project_file_is_announced_at_startup() {
        // Nothing to report when nothing was dropped.
        assert!(startup_notices(Vec::new(), Vec::new(), None, false).is_empty());
        assert!(startup_notices(Vec::new(), Vec::new(), None, true).is_empty());

        // Named as written, after the config warnings and this launch's
        // own, and blaming the knob that actually did it.
        assert_eq!(
            startup_notices(
                vec!["theme ignored".into()],
                vec!["variant dropped".into()],
                Some("AGENTS.md"),
                true
            ),
            vec![
                "theme ignored".to_string(),
                "variant dropped".to_string(),
                "project AGENTS.md present but skipped (--no-project-instructions)".to_string(),
            ]
        );
        assert_eq!(
            startup_notices(Vec::new(), Vec::new(), Some("CLAUDE.md"), false),
            vec![
                "project CLAUDE.md present but skipped (general.project_instructions = false)"
                    .to_string()
            ]
        );
    }

    /// A stash is a place to leave a thought, not a hostage: it rides
    /// into the rebuilt app, so it never blocks a switch. The reasons
    /// that do block still do, in the same order.
    #[test]
    fn a_waiting_stash_does_not_block_a_session_switch() {
        assert_eq!(switch_blocked(false, 0, 0, false, false), None);

        assert_eq!(
            switch_blocked(true, 0, 0, false, false).as_deref(),
            Some("finish or abort the current turn before switching sessions")
        );
        // The refusal names both keys and the count: one that says
        // "abort them first" without saying how is a dead end. Short
        // enough to survive an 80-column notice line, too.
        let agents = switch_blocked(false, 1, 0, false, false).expect("agents block the switch");
        assert!(
            agents.starts_with("1 background task(s) running"),
            "{agents}"
        );
        assert!(agents.contains("Ctrl-Q"), "{agents}");
        assert!(agents.contains("Ctrl-G"), "{agents}");
        assert!(
            agents.len() <= 77,
            "truncates on an 80-column line: {agents}"
        );
        assert_eq!(
            switch_blocked(false, 0, 0, true, false).as_deref(),
            Some("input has an unsent draft; send or clear it first")
        );
        // A goal lives in the app and nowhere else: leaving would drop
        // it, its rounds and its badge silently. /rewind refused
        // already; every other way out now refuses the same way.
        assert_eq!(
            switch_blocked(false, 0, 0, false, true).as_deref(),
            Some("a goal is active — /goal abort before leaving its context")
        );
    }

    /// A command whose `agent:` names nothing is the user's typo, so
    /// the refusal is addressed to the user: it names the frontmatter
    /// key they wrote, not the `subagent_type` JSON field the task
    /// tool would have complained about.
    #[test]
    fn a_command_naming_no_agent_is_refused_in_the_users_own_words() {
        let available = ["build", "explore"];
        assert_eq!(unknown_subtask_agent("explore", &available), None);

        let refusal =
            unknown_subtask_agent("nope", &available).expect("an unknown agent is refused");
        assert!(refusal.contains("no agent named \"nope\""), "{refusal}");
        assert!(refusal.contains("`agent:`"), "{refusal}");
        assert!(refusal.contains("build, explore"), "{refusal}");
        assert!(!refusal.contains("subagent_type"), "{refusal}");
    }

    /// Aborting a turn stops the detached tasks that turn started —
    /// their token is a child of its token — so the notice says so
    /// instead of letting the panel's `bg` rows vanish wordlessly.
    /// With nothing detached there is nothing extra to say.
    #[test]
    fn aborting_a_turn_says_what_happens_to_its_detached_tasks() {
        assert_eq!(abort_notice(0), "aborting current operation…");
        let with_tasks = abort_notice(2);
        assert!(
            with_tasks.starts_with("aborting current operation…"),
            "{with_tasks}"
        );
        assert!(with_tasks.contains("detached task"), "{with_tasks}");
        assert!(with_tasks.contains("held"), "{with_tasks}");
    }

    /// The switch carries the stash, so the pops still work on the
    /// other side — text and images both, newest first.
    #[test]
    fn a_switch_hands_the_stash_to_the_rebuilt_app() {
        let mut before = App::new();
        before.input = InputBuffer::from("the thought worth keeping");
        // Set directly: attachment policy (vision support, caps) is a
        // different test's business than whether the stash carries.
        before.pending_images = vec![ilar::session::ImageContent::png(&[0u8; 32])];
        before.stash_or_pop_input();
        before.input = InputBuffer::from("and another");
        before.stash_or_pop_input();

        // What the exit carries, and what the rebuilt app is handed.
        let carried = std::mem::take(&mut before.input_stash);
        assert!(before.input_stash.is_empty(), "the old app kept a copy");
        let mut after = App::new();
        after.input_stash = carried;

        after.stash_or_pop_input();
        assert_eq!(after.input.text(), "and another");
        after.input.clear();
        after.stash_or_pop_input();
        assert_eq!(after.input.text(), "the thought worth keeping");
        assert_eq!(after.pending_images.len(), 1, "the image came along");
    }

    #[test]
    fn neither_flag_leaves_the_decision_to_configuration() {
        assert_eq!(cli_project_instructions(false, false), None);
        assert_eq!(cli_project_instructions(true, false), Some(true));
        assert_eq!(cli_project_instructions(false, true), Some(false));
        // Clap refuses the pair; if it ever stopped, the file stays out.
        assert_eq!(cli_project_instructions(true, true), Some(false));
    }

    /// The activity drain treats a lag as a gap and keeps going. That
    /// is only right if a lagged broadcast receiver still yields what
    /// came after the drop — pinned here, because the alternative
    /// (breaking out) is what stalled the tape for a frame every time
    /// several children streamed at once.
    #[tokio::test]
    async fn a_lagged_broadcast_yields_the_events_after_the_gap() {
        let (sender, mut receiver) = tokio::sync::broadcast::channel::<u8>(2);
        for value in 0..4 {
            let _ = sender.send(value);
        }
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
        ));
        assert_eq!(receiver.try_recv().unwrap(), 2);
        assert_eq!(receiver.try_recv().unwrap(), 3);
    }

    /// A completion that could not be steered into a dying turn is
    /// requeued and auto-sent as a fresh one — through `StartTurn`,
    /// which used to push it raw. The reader must not be able to tell
    /// which door a notification came in by.
    #[test]
    fn a_requeued_completion_wears_its_collapsed_row_like_a_steered_one() {
        let envelope = "<task-notification>\nTask \"audit\" completed.\n<result>\ndone\n</result>\n</task-notification>";
        let mut app = App::new();
        apply_intent(&mut app, Intent::StartTurn(envelope.into()), None);
        assert!(
            matches!(app.lines().last(), Some(Line_::Task { text, .. }) if text.contains("audit")),
            "a requeued completion showed as something the user typed: {:?}",
            app.lines().last()
        );
    }

    /// Every modal `decide` routes a query paste to must reach its own
    /// filter: the decision and the methods are tested apart, and only
    /// this pins the wiring between them.
    #[test]
    fn a_query_paste_reaches_the_filter_of_the_modal_that_owns_the_keyboard() {
        use modals::{LinkPicker, SessionPickerAction};

        let paste = |app: &mut App, text: &str| {
            let state = decide::LoopState {
                modal: app.active_modal(),
                ..decide::LoopState::default()
            };
            for intent in decide::paste(&state, text.into()) {
                apply_intent(app, intent, None);
            }
        };

        let mut app = App::new();
        app.session_picker = Some(SessionPicker::new(
            vec![ilar::session::SessionSummary {
                id: "aaa".into(),
                title: Some("fix websearch fallback".into()),
                modified: std::time::SystemTime::now(),
                cwd: None,
            }],
            None,
        ));
        paste(&mut app, "websearch");
        let picker = app.session_picker.as_mut().expect("picker open");
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            SessionPickerAction::Resume("aaa".into())
        );
        app.session_picker = None;

        app.session_search = Some(SessionSearch::new());
        paste(&mut app, "needle");
        let search = app.session_search.as_ref().expect("search open");
        assert_eq!(search.query, "needle");
        assert!(search.scanning, "a pasted query must rescan");
        app.session_search = None;

        app.turn_picker = Some(TurnPicker::new(turn_entries(&[
            ilar::session::SessionEvent::UserMessage {
                id: "u1".into(),
                text: "rewrite the parser".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            },
            ilar::session::SessionEvent::UserMessage {
                id: "u2".into(),
                text: "ship the release".into(),
                images: Vec::new(),
                ts: chrono::Utc::now(),
            },
        ])));
        paste(&mut app, "parser");
        let picker = app.turn_picker.as_mut().expect("picker open");
        // Filtered to one turn, so the first Enter arms that one.
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Stay
        );
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            TurnPickerAction::Rewind {
                cut: 0,
                target: "u1".into(),
                discarded: 2,
            }
        );
        app.turn_picker = None;

        app.link_picker = Some(LinkPicker::new(vec![
            links::LinkEntry {
                label: "docs".into(),
                url: "https://docs.example/one".into(),
            },
            links::LinkEntry {
                label: "issue tracker".into(),
                url: "https://bugs.example/two".into(),
            },
        ]));
        paste(&mut app, "tracker");
        let picker = app.link_picker.as_mut().expect("picker open");
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("https://bugs.example/two".into())
        );
        app.link_picker = None;

        app.model_picker = Some(ModelPicker::new(
            ilar::model::catalog().iter().collect(),
            "missing/model",
        ));
        paste(&mut app, "glm-4.7");
        let picker = app.model_picker.as_mut().expect("picker open");
        assert_eq!(
            picker.handle_key(KeyCode::Enter, false),
            PickerAction::Choose("zai/glm-4.7".into())
        );
        app.model_picker = None;

        // The theme picker previews as it filters, so the paste has to
        // land on the app's live theme too.
        app.theme_picker = Some(ThemePicker::new(app.theme));
        paste(&mut app, "gruv");
        assert!(app.theme.id().contains("gruv"), "{}", app.theme.id());
        app.theme_picker = None;

        // A modal with no text field still swallows it rather than
        // typing into the prompt behind it.
        app.help_visible = true;
        paste(&mut app, "into the void");
        assert!(app.input.is_blank(), "help let a paste through");
    }

    #[test]
    fn the_window_title_is_the_topic_or_just_ilar() {
        assert_eq!(terminal_title(None), "ilar");
        assert_eq!(
            terminal_title(Some("GM1 firmware dig")),
            "ilar — GM1 firmware dig"
        );
    }

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
        // Both are shown verbatim as `you` rows: source indentation
        // must not reach the transcript as runs of spaces.
        for prompt in [&kickoff, &cont] {
            assert!(!prompt.contains("  "), "{prompt}");
        }
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
                    cwd: None,
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
            cwd: None,
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
                cwd: None,
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
                cwd: None,
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
                    cwd: None,
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
    fn queued_motion_events_collapse_to_the_newest_position() {
        let moved = |column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let key = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let mut queued = vec![moved(4, 5), moved(9, 9), key, moved(1, 1)].into_iter();

        let batch = drain_motion_batch((2, 2), 32, || Ok(queued.next())).unwrap();

        assert_eq!((batch.column, batch.row), (9, 9));
        assert!(matches!(
            batch.deferred,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Char('x'),
                ..
            }))
        ));
        // The event behind the deferred one stays queued for later.
        assert!(matches!(
            queued.next(),
            Some(Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Moved,
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
                cwd: None,
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
                images: Vec::new(),
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

    /// Focus opens on the store's replay of the child session — the
    /// same seed the picker preview trusts — and a session that will
    /// not load is an honest notice, never a blank screen.
    #[test]
    fn focus_opens_from_the_store_and_refuses_a_missing_session_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        let mut session = store
            .create(SessionMeta {
                session_id: session_id.clone(),
                parent_id: None,
                agent: "explore".into(),
                model: "zai/glm-4.7".into(),
                workspace: None,
                cwd: None,
            })
            .unwrap();
        session
            .append(ilar::session::SessionEvent::AssistantMessage {
                id: new_id(),
                model: "zai/glm-4.7".into(),
                content: vec![ilar::session::ContentBlock::Text {
                    text: "replayed child reply".into(),
                }],
                usage: ilar::session::Usage::default(),
                stop_reason: "end_turn".into(),
                ts: chrono::Utc::now(),
            })
            .unwrap();
        drop(session);

        // Listed in the roster: the row lends its title and running.
        let mut app = App::new();
        app.agents_view = vec![AgentRow {
            session_id: session_id.clone(),
            depth: 0,
            description: "survey the picker core".into(),
            agent: "explore".into(),
            background: false,
            delivering: false,
            foreign_parent: None,
            elapsed: std::time::Duration::from_secs(1),
            waiting: false,
            quiet: None,
        }];
        let streaming = open_agent_focus(&mut app, &store, &session_id).expect("a fresh focus");
        land_agent_focus(
            &mut app,
            &session_id,
            seed_agent_focus(&store, &session_id, streaming),
        );
        let focus = app.focus.as_ref().expect("focus opened");
        assert_eq!(focus.title, "explore · survey the picker core");
        assert!(focus.running);
        assert!(
            focus
                .lines
                .iter()
                .any(|line| matches!(line, Line_::Assistant(text) if text.contains("replayed child reply"))),
            "{:?}",
            focus.lines
        );

        // Off the roster — finished, or a foreign tree pruned away —
        // the replay still opens, marked not running.
        app.agents_view.clear();
        app.close_focus();
        let streaming = open_agent_focus(&mut app, &store, &session_id).expect("a fresh focus");
        land_agent_focus(
            &mut app,
            &session_id,
            seed_agent_focus(&store, &session_id, streaming),
        );
        assert!(app.focus.as_ref().is_some_and(|focus| !focus.running));

        // A session the store cannot load: a notice, no focus.
        app.close_focus();
        let streaming =
            open_agent_focus(&mut app, &store, "no-such-session").expect("a fresh focus");
        land_agent_focus(
            &mut app,
            "no-such-session",
            seed_agent_focus(&store, "no-such-session", streaming),
        );
        assert!(app.focus.is_none(), "a blank screen is not an answer");
        let (notice, _) = app.operational_notice().expect("an honest notice");
        assert!(notice.contains("cannot open agent transcript"), "{notice}");
    }

    /// The preview must show why a row matched: the window centers on
    /// the match even when it sits deep inside a long entry, with the
    /// cut ends marked.
    #[test]
    fn the_hit_window_centers_on_the_match() {
        let text = format!("{}NEEDLE{}", "a".repeat(2000), "b".repeat(2000));
        let window = center_on_match(&text, "needle", 100);
        assert!(window.contains("NEEDLE"), "{window}");
        assert!(window.starts_with('…') && window.ends_with('…'), "{window}");

        let early = center_on_match("needle right at the front", "needle", 100);
        assert_eq!(early, "needle right at the front");
    }

    #[test]
    fn held_notifications_come_back_before_the_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let note = |description: &str| ilar::subagent::Notification {
            parent_session_id: "parent".into(),
            description: description.into(),
            text: description.into(),
            is_error: false,
        };
        tx.try_send(note("from the channel")).unwrap();
        let mut held =
            std::collections::VecDeque::from([ilar::delivery::Parcel::fresh(note("held earlier"))]);

        assert_eq!(
            next_notification(&mut held, &mut rx)
                .unwrap()
                .notification()
                .description,
            "held earlier",
            "a held notification arrived first and is offered first"
        );
        assert_eq!(
            next_notification(&mut held, &mut rx)
                .unwrap()
                .notification()
                .description,
            "from the channel"
        );
        assert!(next_notification(&mut held, &mut rx).is_none());
    }

    /// Opening a session is reading, not summoning: a recovered
    /// backlog is adopted in order but paused, so nothing starts a
    /// turn before the user's first message. No backlog, no pause —
    /// a fresh open behaves exactly as before.
    #[test]
    fn a_recovered_backlog_is_adopted_but_held_for_the_user() {
        let note = |description: &str| ilar::subagent::Notification {
            parent_session_id: "parent".into(),
            description: description.into(),
            text: description.into(),
            is_error: false,
        };

        let (held, paused) = adopt_recovered(vec![note("first"), note("second")]);
        assert!(paused, "a backlog must wait for the user");
        assert_eq!(
            held.iter()
                .map(|parcel| parcel.notification().description.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "adoption preserves the recorded order"
        );

        let (held, paused) = adopt_recovered(Vec::new());
        assert!(!paused, "an empty recovery opens the gate as before");
        assert!(held.is_empty());
    }

    /// A propagated completion must not jump the queue: everything the
    /// channel already held is offered first. A bare push_back fails
    /// this, because held notifications outrank the channel.
    #[test]
    fn propagated_notification_follows_the_existing_receiver_backlog() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let note = |description: &str| ilar::subagent::Notification {
            parent_session_id: "parent".into(),
            description: description.into(),
            text: description.into(),
            is_error: false,
        };
        tx.try_send(note("queued")).unwrap();
        let mut held = std::collections::VecDeque::new();

        hold_propagate_behind_backlog(
            &mut held,
            &mut rx,
            ilar::delivery::Parcel::fresh(note("propagated")),
        );

        assert_eq!(
            next_notification(&mut held, &mut rx)
                .unwrap()
                .notification()
                .description,
            "queued"
        );
        assert_eq!(
            next_notification(&mut held, &mut rx)
                .unwrap()
                .notification()
                .description,
            "propagated"
        );
    }

    /// A command's one-turn model override belongs to the turn that
    /// command started — including the stretch of it after the model
    /// paused to ask a question. The answer resumes the same turn, so
    /// it adopts the override the same way a fresh prompt does; a
    /// retry-resume, which is not that command's turn, leaves it
    /// pending.
    #[test]
    fn a_question_answer_adopts_a_pending_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let session_id = new_id();
        drop(
            store
                .create(SessionMeta {
                    session_id: session_id.clone(),
                    parent_id: None,
                    agent: "build".into(),
                    model: "zai/glm-4.7".into(),
                    workspace: None,
                    cwd: None,
                })
                .unwrap(),
        );
        let resolver: Arc<dyn ProviderResolver> = Arc::new(ilar::provider::MockProvider::default());
        let registry = ToolRegistry::read_only();
        let tool_ctx = ToolContext::root(dir.path().to_path_buf());
        let loop_config = LoopConfig::default();
        let deps = TurnDeps {
            resolver: &resolver,
            store: &store,
            session_id: &session_id,
            system_prompt: "",
            registry: &registry,
            tool_ctx: &tool_ctx,
            loop_config: &loop_config,
        };
        let pending = || Some((Some("openai/gpt-5.2".to_string()), Some("high".to_string())));

        let mut app = App::new();
        app.current_model = "zai/glm-4.7".into();
        app.pending_model_override = pending();

        adopt_pending_model_override(
            &mut app,
            &RootTurn::Answer(ilar::question::QuestionResponse::Cancelled),
            &deps,
        );

        assert_eq!(app.current_model, "openai/gpt-5.2");
        assert_eq!(app.current_variant.as_deref(), Some("high"));
        assert_eq!(
            store.load(&session_id).unwrap().effective_model(),
            "openai/gpt-5.2"
        );
        assert_eq!(
            app.model_revert,
            Some(("zai/glm-4.7".to_string(), None)),
            "the answer turn must still hand the session back afterwards"
        );
        assert!(app.pending_model_override.is_none());

        // A retry-resume is a continuation of a turn that already
        // adopted whatever it was going to adopt.
        app.pending_model_override = pending();
        adopt_pending_model_override(&mut app, &RootTurn::Resume, &deps);
        assert_eq!(app.pending_model_override, pending());
    }
}
