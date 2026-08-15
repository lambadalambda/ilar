//! ilar TUI: transcript, streaming, tool display, input. Esc aborts.

mod markdown;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
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
    Tool {
        id: String,
        text: String,
        done: bool,
    },
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

struct TerminalSession;

impl TerminalSession {
    fn start() -> Result<(ratatui::DefaultTerminal, Self)> {
        let terminal = ratatui::init();
        if let Err(error) = crossterm::execute!(std::io::stdout(), EnableMouseCapture) {
            ratatui::restore();
            return Err(error.into());
        }
        Ok((terminal, Self))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}

struct App {
    lines: Vec<Line_>,
    input: String,
    busy: bool,
    status: String,
    scroll_top: usize,
    content_rows: usize,
    viewport_rows: usize,
    follow_tail: bool,
}

impl App {
    fn new() -> Self {
        Self {
            lines: vec![Line_::System(
                "ilar — PgUp/PgDn scroll, Ctrl-End follows tail, Esc aborts, Ctrl-C quits".into(),
            )],
            input: String::new(),
            busy: false,
            status: String::new(),
            scroll_top: 0,
            content_rows: 0,
            viewport_rows: 0,
            follow_tail: true,
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
            LoopEvent::ToolStarted { id, name } => {
                self.lines.push(Line_::Tool {
                    id: id.clone(),
                    text: format!("▶ {name} …"),
                    done: false,
                });
                self.status = format!("running {name}");
            }
            LoopEvent::ToolFinished { id, name, is_error } => {
                let mut matched = false;
                if let Some((text, done)) =
                    self.lines.iter_mut().rev().find_map(|line| match line {
                        Line_::Tool {
                            id: line_id,
                            text,
                            done,
                        } if line_id == id && !*done => Some((text, done)),
                        _ => None,
                    })
                {
                    *text = format!(
                        "{} {}",
                        text.trim_end_matches('…').trim_end(),
                        if *is_error { "✗" } else { "✓" }
                    );
                    *done = true;
                    matched = true;
                }
                if !matched {
                    self.lines.push(Line_::Tool {
                        id: id.clone(),
                        text: format!("▪ {name} {}", if *is_error { "✗" } else { "✓" }),
                        done: true,
                    });
                }
                let running = self
                    .lines
                    .iter()
                    .filter(|line| matches!(line, Line_::Tool { done: false, .. }))
                    .count();
                self.status = match running {
                    0 => "thinking".into(),
                    1 => "running 1 tool".into(),
                    count => format!("running {count} tools"),
                };
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

    fn finish_turn(&mut self, result: anyhow::Result<TurnOutcome>) {
        if let Err(error) = result {
            self.lines.push(Line_::System(format!("error: {error:#}")));
            self.status = "error".into();
        }
        self.busy = false;
    }

    fn max_scroll(&self) -> usize {
        self.content_rows.saturating_sub(self.viewport_rows)
    }

    fn page_size(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    fn scroll_up(&mut self, rows: usize) {
        self.follow_tail = false;
        self.scroll_top = self.scroll_top.saturating_sub(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        let max_scroll = self.max_scroll();
        self.scroll_top = self.scroll_top.saturating_add(rows).min(max_scroll);
        self.follow_tail = self.scroll_top == max_scroll;
    }

    fn scroll_to_top(&mut self) {
        self.scroll_top = 0;
        self.follow_tail = self.max_scroll() == 0;
    }

    fn scroll_to_tail(&mut self) {
        self.scroll_top = self.max_scroll();
        self.follow_tail = true;
    }

    fn update_scroll_metrics(&mut self, content_rows: usize, viewport_rows: usize) {
        self.content_rows = content_rows;
        self.viewport_rows = viewport_rows;
        let max_scroll = self.max_scroll();
        if self.follow_tail {
            self.scroll_top = max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(max_scroll);
        }
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut output = Vec::new();
        for entry in &self.lines {
            match entry {
                Line_::Assistant(text) => {
                    for (index, mut line) in markdown::render(text).into_iter().enumerate() {
                        let label = if index == 0 { "ilar " } else { "     " };
                        let mut spans = vec![Span::styled(
                            label,
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        )];
                        spans.append(&mut line.spans);
                        output.push(Line::from(spans));
                    }
                }
                Line_::User(text) => {
                    for (index, text) in safe_lines(text).into_iter().enumerate() {
                        output.push(Line::from(vec![
                            Span::styled(
                                if index == 0 { "you  " } else { "     " },
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::raw(text),
                        ]));
                    }
                }
                Line_::Tool { text, .. } => output.push(Line::from(vec![
                    Span::styled("tool ", Style::default().fg(Color::Yellow)),
                    Span::raw(safe_text(text)),
                ])),
                Line_::System(text) => {
                    for (index, text) in safe_lines(text).into_iter().enumerate() {
                        output.push(Line::from(vec![
                            Span::styled(
                                if index == 0 { "—    " } else { "     " },
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(text, Style::default().fg(Color::DarkGray)),
                        ]));
                    }
                }
            }
        }
        output
    }

    fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(frame.area());

        let text = self.transcript_lines();
        let transcript_area = chunks[0];
        let text_width = transcript_area.width.saturating_sub(3);
        let viewport_rows = transcript_area.height.saturating_sub(2) as usize;
        let content_rows = Paragraph::new(text.clone())
            .wrap(Wrap { trim: false })
            .line_count(text_width);
        self.update_scroll_metrics(content_rows, viewport_rows);
        let max_scroll = self.max_scroll();
        let scroll_label = if max_scroll == 0 {
            String::new()
        } else if self.follow_tail {
            " · tail".into()
        } else {
            format!(" · {}%", self.scroll_top.saturating_mul(100) / max_scroll)
        };
        let paragraph = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("ilar{scroll_label}"))
                    .padding(Padding::new(0, 1, 0, 0)),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_top.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, transcript_area);

        if max_scroll > 0 && transcript_area.height > 2 {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃");
            let mut state = ScrollbarState::new(max_scroll.saturating_add(1))
                .position(self.scroll_top)
                .viewport_content_length(self.viewport_rows);
            let area = Rect::new(
                transcript_area.right().saturating_sub(2),
                transcript_area.y.saturating_add(1),
                1,
                transcript_area.height.saturating_sub(2),
            );
            frame.render_stateful_widget(scrollbar, area, &mut state);
        }

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

fn safe_text(text: &str) -> String {
    let mut output = String::new();
    let mut column = 0usize;
    for character in text.chars().filter(|c| *c == '\t' || !c.is_control()) {
        if character == '\t' {
            let spaces = 4 - column % 4;
            output.push_str(&" ".repeat(spaces));
            column += spaces;
        } else {
            output.push(character);
            column += 1;
        }
    }
    output
}

fn safe_lines(text: &str) -> Vec<String> {
    let lines: Vec<_> = text.lines().map(safe_text).collect();
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
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
        .with_subagents(spawner.clone())?
        .with_todos(todos)?
        .with_web_tools()?
        .with_skills(skill_store)?;
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
        if let Some(openai) = config.providers.get("openai") {
            if openai.auth.as_deref() == Some("chatgpt") {
                choices.push("openai/gpt-5.6-sol".to_string());
            } else if openai.api_key.is_some() {
                choices.push("openai/gpt-5.2".to_string());
            }
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

    let (mut terminal, _terminal_session) = TerminalSession::start()?;
    run_app(
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
    .await
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
            let result = match handle.await {
                Ok(result) => result,
                Err(error) => Err(error.into()),
            };
            if let Some(rx) = events_rx.as_mut() {
                while let Ok(event) = rx.try_recv() {
                    app.push_loop_event(&event);
                }
            }
            app.finish_turn(result);
            events_rx = None;
            cancel = None;
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

        terminal.draw(|frame| app.render(frame))?;

        // Poll terminal input (fast while busy so streaming keeps rendering).
        let timeout = if app.busy {
            std::time::Duration::from_millis(50)
        } else {
            std::time::Duration::from_millis(250)
        };
        if !crossterm::event::poll(timeout)? {
            continue;
        }
        match crossterm::event::read()? {
            Event::Key(KeyEvent {
                code,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                modifiers,
                ..
            }) => match (code, modifiers.contains(KeyModifiers::CONTROL)) {
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
                        app.follow_tail = true;
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
                (KeyCode::Char('u'), true) => {
                    app.scroll_up(app.page_size().div_ceil(2));
                }
                (KeyCode::Char('d'), true) => {
                    app.scroll_down(app.page_size().div_ceil(2));
                }
                (KeyCode::Home, true) => app.scroll_to_top(),
                (KeyCode::End, true) => app.scroll_to_tail(),
                (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                (KeyCode::Up, _) => app.scroll_up(1),
                (KeyCode::Down, _) => app.scroll_down(1),
                (KeyCode::Char(c), false) => {
                    app.input.push(c);
                }
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => app.scroll_up(3),
                MouseEventKind::ScrollDown => app.scroll_down(3),
                _ => {}
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_error_is_visible() {
        let mut app = App::new();
        app.busy = true;

        app.finish_turn(Err(anyhow::anyhow!("provider rejected tool result")));

        assert!(!app.busy);
        assert_eq!(app.status, "error");
        assert!(matches!(
            app.lines.last(),
            Some(Line_::System(message)) if message.contains("provider rejected tool result")
        ));
    }

    #[test]
    fn parallel_tool_completions_match_by_id() {
        let mut app = App::new();
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "read-1".into(),
            name: "read".into(),
        });
        app.push_loop_event(&LoopEvent::ToolStarted {
            id: "todo-1".into(),
            name: "todo".into(),
        });
        app.push_loop_event(&LoopEvent::ToolFinished {
            id: "read-1".into(),
            name: "read".into(),
            is_error: false,
        });

        assert!(matches!(
            &app.lines[1],
            Line_::Tool { id, done: true, .. } if id == "read-1"
        ));
        assert!(matches!(
            &app.lines[2],
            Line_::Tool { id, done: false, .. } if id == "todo-1"
        ));
    }

    #[test]
    fn scrolling_detaches_and_resumes_tail_follow() {
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 80;
        app.follow_tail = true;

        app.scroll_up(18);
        assert_eq!(app.scroll_top, 62);
        assert!(!app.follow_tail);

        app.scroll_down(18);
        assert_eq!(app.scroll_top, 80);
        assert!(app.follow_tail);
    }

    #[test]
    fn scrolling_clamps_at_top_and_bottom() {
        let mut app = App::new();
        app.content_rows = 30;
        app.viewport_rows = 10;
        app.scroll_top = 20;

        app.scroll_up(100);
        assert_eq!(app.scroll_top, 0);
        app.scroll_down(100);
        assert_eq!(app.scroll_top, 20);
        assert!(app.follow_tail);
    }

    #[test]
    fn resize_clamps_without_reattaching_detached_viewport() {
        let mut app = App::new();
        app.content_rows = 100;
        app.viewport_rows = 20;
        app.scroll_top = 70;
        app.follow_tail = false;

        app.update_scroll_metrics(30, 20);

        assert_eq!(app.scroll_top, 10);
        assert!(!app.follow_tail);
    }
}
