//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio_util::sync::CancellationToken;

use ilar::agent::{LoopConfig, LoopEvent, TurnOutcome, run_turn};
use ilar::config::{Loader, system_prompt_for};
use ilar::provider::Provider;
use ilar::session::{SessionMeta, SessionStore, new_id};
use ilar::subagent::SubagentSpawner;
use ilar::tools::{ToolContext, ToolRegistry};

/// A rendered line in the transcript.
#[derive(Debug, Clone)]
enum Line_ {
    User(String),
    Assistant(String),
    Tool(String, bool),
    System(String),
}

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

    /// Agent name from config (markdown agents).
    #[arg(long, default_value = "build")]
    agent: String,

    /// Print the resolved system prompt and exit (debugging).
    #[arg(long)]
    print_prompt: bool,
}

struct App {
    lines: Vec<Line_>,
    input: String,
    busy: bool,
    status: String,
    scroll: u16,
}

impl App {
    fn new() -> Self {
        Self {
            lines: vec![Line_::System(
                "ilar — type a request, Esc aborts, Ctrl-C quits".into(),
            )],
            input: String::new(),
            busy: false,
            status: String::new(),
            scroll: 0,
        }
    }

    fn push_loop_event(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::TurnStarted => {
                self.status = "thinking…".into();
            }
            LoopEvent::TextDelta(t) => {
                self.status = "streaming".into();
                match self.lines.last_mut() {
                    Some(Line_::Assistant(text)) => text.push_str(t),
                    _ => self.lines.push(Line_::Assistant(t.clone())),
                }
            }
            LoopEvent::ThinkingDelta(_) => {
                self.status = "thinking".into();
            }
            LoopEvent::ToolStarted { name, .. } => {
                self.lines.push(Line_::Tool(format!("▶ {name} …"), false));
                self.status = format!("running {name}");
            }
            LoopEvent::ToolFinished { name, is_error, .. } => {
                if let Some(Line_::Tool(text, done)) = self.lines.last_mut()
                    && !*done
                {
                    *text = format!(
                        "{} {}",
                        text.trim_end_matches("…"),
                        if *is_error { "✗" } else { "✓" }
                    );
                    *done = true;
                    return;
                }
                self.lines.push(Line_::Tool(
                    format!("▪ {name} {}", if *is_error { "✗" } else { "✓" }),
                    true,
                ));
            }
            LoopEvent::StepComplete { stop_reason, usage } => {
                self.status = format!(
                    "{stop_reason} · in {} out {} (cache {})",
                    usage.input_tokens, usage.output_tokens, usage.cache_read_input_tokens
                );
            }
            LoopEvent::Compacted => {
                self.lines
                    .push(Line_::System("transcript compacted".into()));
            }
            LoopEvent::TurnDone { outcome } => {
                self.busy = false;
                self.status = match outcome {
                    TurnOutcome::Completed => "ready".into(),
                    TurnOutcome::Aborted => "aborted".into(),
                    TurnOutcome::MaxIterations => "stopped: max iterations".into(),
                };
            }
        }
    }

    fn render(&self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(frame.area());

        let text: Vec<Line> = self
            .lines
            .iter()
            .map(|line| match line {
                Line_::User(t) => Line::from(vec![
                    Span::styled(
                        "you  ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(t.clone()),
                ]),
                Line_::Assistant(t) => Line::from(vec![
                    Span::styled(
                        "ilar ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(t.clone()),
                ]),
                Line_::Tool(t, _) => Line::from(vec![
                    Span::styled("tool ", Style::default().fg(Color::Yellow)),
                    Span::raw(t.clone()),
                ]),
                Line_::System(t) => Line::from(vec![
                    Span::styled("—    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(t.clone(), Style::default().fg(Color::DarkGray)),
                ]),
            })
            .collect();

        // Wrap assistant lines lazily via Paragraph's Wrap instead of manual
        // splitting: we render one logical line per entry, wrapped.
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("ilar"))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(paragraph, chunks[0]);

        let busy = if self.busy {
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::DarkGray),
            )
        };
        frame.render_widget(Paragraph::new(Line::from(busy)), chunks[1]);

        let input = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title("input"))
            .wrap(Wrap { trim: false });
        frame.render_widget(input, chunks[2]);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(Command::Login) = args.command {
        let store = ilar::auth::AuthStore::open(ilar::config::default_state_dir());
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

    let config = Loader::new().resolve().context("loading config")?;
    let model = args
        .model
        .clone()
        .or_else(|| Some(config.general.model.clone()))
        .unwrap();
    let agent = config
        .agents()
        .into_iter()
        .find(|a| a.name == args.agent)
        .with_context(|| format!("unknown agent {:?}", args.agent))?;

    let cwd = std::env::current_dir().context("no cwd")?;
    let skill_store = std::sync::Arc::new(ilar::skill::SkillStore::new(
        config.dirs().0.to_path_buf(),
        cwd.clone(),
    ));
    let skill_listing = skill_store.listing_prompt();
    let mut system_prompt = system_prompt_for(&cwd);
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

    let model_for_session = agent.model.clone().unwrap_or_else(|| model.clone());
    let provider: Arc<dyn Provider> = config
        .provider_for(&model_for_session)
        .with_context(|| format!("no provider configured for {model_for_session} (set ILAR_ZAI_API_KEY / ILAR_OPENAI_API_KEY)"))?
        .into();

    let state_dir = ilar::config::default_state_dir();
    let store = SessionStore::new(state_dir.join("sessions"));
    let session_id = match &args.session {
        Some(id) => id.clone(),
        None => {
            let id = new_id();
            store
                .create(SessionMeta {
                    session_id: id.clone(),
                    parent_id: None,
                    agent: agent.name.clone(),
                    model: model_for_session.clone(),
                })
                .context("creating session")?;
            id
        }
    };

    let agents = config.agents();
    let spawner = std::sync::Arc::new(SubagentSpawner::new(
        provider.clone(),
        store.clone(),
        agents,
        cwd.clone(),
        0,
        config.subagents.max_concurrent,
        config.subagents.max_depth,
    ));
    let todos = std::sync::Arc::new(std::sync::Mutex::new(ilar::todo::TodoList::default()));
    let registry = ToolRegistry::builtin()
        .with_subagents(spawner.clone())
        .with_todos(todos)
        .with_web_tools()
        .with_skills(skill_store);
    let notifications = spawner.subscribe();
    let tool_ctx = ToolContext::root(cwd.clone()).with_subagents(spawner.clone());
    let model_choices: Vec<String> = {
        let mut choices = Vec::new();
        if config
            .providers
            .get("zai")
            .and_then(|p| p.api_key.as_ref())
            .is_some()
        {
            choices.extend(["zai/glm-4.7".to_string(), "zai/glm-4.7-air".to_string()]);
        }
        if config
            .providers
            .get("openai")
            .and_then(|p| p.api_key.as_ref())
            .is_some()
        {
            choices.push("openai/gpt-5.2".to_string());
        }
        if !choices.contains(&model_for_session) {
            choices.insert(0, model_for_session.clone());
        }
        choices
    };

    let loop_config = {
        let threshold = config.compaction.threshold;
        // GLM context window ~200k; conservative default per provider.
        let limit = model_for_session
            .starts_with("zai/")
            .then_some(200_000u64)
            .or(Some(128_000));
        move || LoopConfig {
            context_limit: limit,
            compaction_threshold: threshold,
            ..LoopConfig::default()
        }
    };
    let mut app = App::new();
    app.status = format!("{model_for_session} · {session_id}");

    let mut terminal = ratatui::init();
    let result = run_app(
        &mut terminal,
        &mut app,
        provider,
        &store,
        &session_id,
        &system_prompt,
        &registry,
        tool_ctx,
        spawner,
        notifications,
        loop_config,
        model_choices,
        config,
    )
    .await;
    ratatui::restore();
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_app(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    provider: Arc<dyn Provider>,
    store: &SessionStore,
    session_id: &str,
    system_prompt: &str,
    registry: &ToolRegistry,
    tool_ctx: ToolContext,
    spawner: std::sync::Arc<ilar::subagent::SubagentSpawner>,
    mut notifications: tokio::sync::mpsc::UnboundedReceiver<ilar::subagent::Notification>,
    loop_config: impl Fn() -> LoopConfig + Clone,
    model_choices: Vec<String>,
    config: ilar::config::Config,
) -> Result<()> {
    let mut events_rx: Option<tokio::sync::mpsc::UnboundedReceiver<LoopEvent>> = None;
    let mut provider = provider;
    let mut model_index = 0usize;
    let mut cancel: Option<CancellationToken> = None;
    let mut turn_handle: Option<tokio::task::JoinHandle<Result<TurnOutcome>>> = None;

    loop {
        terminal.draw(|frame| app.render(frame))?;

        // Drain pending loop events.
        if let Some(rx) = events_rx.as_mut() {
            while let Ok(event) = rx.try_recv() {
                app.push_loop_event(&event);
            }
        }
        // Turn finished? Join and clean up.
        if let Some(handle) = turn_handle.as_mut()
            && handle.is_finished()
        {
            let handle = turn_handle.take().unwrap();
            let result = handle.await;
            if let Err(e) = result {
                app.lines.push(Line_::System(format!("error: {e:#}")));
            }
            events_rx = None;
            cancel = None;
            app.busy = false;
        }

        // Background completions re-invoke the loop while idle.
        if !app.busy {
            while let Ok(notification) = notifications.try_recv() {
                app.lines.push(Line_::System(format!(
                    "task notification: {}",
                    notification.description
                )));
                let text = notification.text;
                app.lines.push(Line_::User(text.clone()));
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                events_rx = Some(rx);
                let token = CancellationToken::new();
                cancel = Some(token.clone());
                app.busy = true;
                let provider = provider.clone();
                let store = store.clone();
                let session_id = session_id.to_string();
                let system_prompt = system_prompt.to_string();
                let registry = registry.clone();
                let turn_ctx = tool_ctx.clone();
                let loop_config = loop_config();
                turn_handle = Some(tokio::spawn(async move {
                    run_turn(
                        provider.as_ref(),
                        &registry,
                        &store,
                        &session_id,
                        &text,
                        Some(&system_prompt),
                        loop_config,
                        tx,
                        token,
                        turn_ctx,
                    )
                    .await
                }));
            }
        }

        // Poll terminal input (fast while busy so streaming keeps rendering).
        let timeout = if app.busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        if crossterm::event::poll(timeout)?
            && let Ok(Event::Key(KeyEvent {
                code,
                kind: KeyEventKind::Press,
                modifiers,
                ..
            })) = crossterm::event::read()
        {
            match (code, modifiers.contains(KeyModifiers::CONTROL)) {
                (KeyCode::Char('c'), true) => {
                    if let Some(cancel) = &cancel {
                        cancel.cancel();
                    }
                    spawner.abort_all();
                    return Ok(());
                }
                (KeyCode::Char('m'), true) if !app.busy && model_choices.len() > 1 => {
                    model_index = (model_index + 1) % model_choices.len();
                    let new_model = model_choices[model_index].clone();
                    match config.provider_for(&new_model) {
                        Some(new_provider) => {
                            provider = new_provider.into();
                            if let Ok(mut session) = store.load(session_id) {
                                let _ = session.append(ilar::session::SessionEvent::ModelChange {
                                    id: ilar::session::new_id(),
                                    model: new_model.clone(),
                                    ts: chrono::Utc::now(),
                                });
                            }
                            app.status = format!("model: {new_model}");
                            app.lines
                                .push(Line_::System(format!("switched to {new_model}")));
                        }
                        None => {
                            app.lines
                                .push(Line_::System(format!("no provider for {new_model}")));
                        }
                    }
                }
                (KeyCode::Esc, _) => {
                    if app.busy {
                        if let Some(cancel) = &cancel {
                            cancel.cancel();
                            app.status = "aborting…".into();
                        }
                    } else {
                        app.input.clear();
                    }
                }
                (KeyCode::Enter, _) => {
                    if !app.busy && !app.input.trim().is_empty() {
                        let text = std::mem::take(&mut app.input);
                        app.lines.push(Line_::User(text.clone()));
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        events_rx = Some(rx);
                        let token = CancellationToken::new();
                        cancel = Some(token.clone());
                        app.busy = true;
                        let provider = provider.clone();
                        let store = store.clone();
                        let session_id = session_id.to_string();
                        let system_prompt = system_prompt.to_string();
                        let registry = registry.clone();
                        let turn_ctx = tool_ctx.clone();
                        turn_handle = Some(tokio::spawn(async move {
                            run_turn(
                                provider.as_ref(),
                                &registry,
                                &store,
                                &session_id,
                                &text,
                                Some(&system_prompt),
                                LoopConfig::default(),
                                tx,
                                token,
                                turn_ctx,
                            )
                            .await
                        }));
                    }
                }
                (KeyCode::Backspace, _) => {
                    app.input.pop();
                }
                (KeyCode::Char(c), _) => {
                    app.input.push(c);
                }
                (KeyCode::PageUp, _) => {
                    app.scroll = app.scroll.saturating_add(5);
                }
                (KeyCode::PageDown, _) => {
                    app.scroll = app.scroll.saturating_sub(5);
                }
                _ => {}
            }
        }
    }
}
