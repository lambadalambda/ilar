//! `ilar --view <session>`: a session on screen, read-only.
//!
//! The transcript is the TUI's own — the same restore the picker
//! does, the same renderer — but nothing drives the session: no
//! runtime, no writer lease, no provider. The file is followed, and
//! every change rebuilds the view, so a gateway chat or another TUI
//! can be watched while it works. Enter does nothing but say so.

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ilar::config::Config;
use ilar::session::{SessionStore, SessionTail};

use crate::app::{App, NoticeLevel};
use crate::session_view::{Liveness, restored_session_view_with_store};
use crate::transcript::Line_;

const POLL: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) async fn run(config: &Config, id: &str, theme: crate::theme::ThemeId) -> Result<()> {
    let store = ilar::runtime::session_store(config);
    store.load(id).with_context(|| format!("no session {id}"))?;
    let mut tail = SessionTail::open(&store, id).with_context(|| format!("following {id}"))?;
    let (mut terminal, _session) = crate::TerminalSession::start()?;
    let mut app = App::new();
    app.theme = theme;
    app.session_id = id.to_string();
    app.status = "read-only view".into();
    app.push_transcript_line(Line_::System(format!(
        "read-only view of session {id} — follows the file; q or Esc to leave"
    )));
    reload(&mut app, &store, id)?;
    loop {
        terminal.draw(|frame| app.render(frame))?;
        if crossterm::event::poll(POLL)? {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    match (key.code, control) {
                        (KeyCode::Char('c'), true)
                        | (KeyCode::Char('q'), false)
                        | (KeyCode::Esc, false) => {
                            break;
                        }
                        (KeyCode::Up, _) => app.scroll_up(1),
                        (KeyCode::Down, _) => app.scroll_down(1),
                        (KeyCode::PageUp, _) => app.scroll_up(app.page_size()),
                        (KeyCode::PageDown, _) => app.scroll_down(app.page_size()),
                        (KeyCode::Home, _) => app.scroll_to_top(),
                        (KeyCode::End, _) => app.scroll_to_tail(),
                        (KeyCode::Enter, _) => app.set_notice(
                            "read-only view: open the session without --view to talk to it",
                            NoticeLevel::Warning,
                        ),
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_wheel(-3),
                    MouseEventKind::ScrollDown => app.scroll_wheel(3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        app.begin_transcript_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        // A drag selects; a click expands a fold. The
                        // selected text goes nowhere here.
                        let _ = app.finish_transcript_selection(mouse.column, mouse.row);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if !tail.poll()?.is_empty() {
            reload(&mut app, &store, id)?;
        }
    }
    Ok(())
}

/// The whole view again from the log. A rebuild rather than an
/// append: the restore knows the folds, the rewinds and the children,
/// and a change is rare enough that re-reading the log is fine.
fn reload(app: &mut App, store: &SessionStore, id: &str) -> Result<()> {
    let reader = store.load(id)?;
    let liveness = if driven(store, id) {
        Liveness::Running
    } else {
        Liveness::Settled
    };
    let view = restored_session_view_with_store(&reader, store, liveness);
    app.replace_transcript(view);
    Ok(())
}

/// Whether something holds the session's writer lease right now. The
/// probe takes the lease for an instant when it is free; a turn
/// arriving in that instant waits, it is not refused.
fn driven(store: &SessionStore, id: &str) -> bool {
    match store.acquire_writer(id) {
        Ok(_writer) => false,
        Err(error) => ilar::agent::TurnNeverStarted::writer_held(&anyhow::Error::from(error)),
    }
}
