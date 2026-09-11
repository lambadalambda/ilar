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

use crate::NoticeLevel;
use crate::app::App;
use crate::session_view::{Liveness, RestoredSessionView, restored_session_view_with_store};
use crate::transcript::Line_;

const POLL: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) async fn run(config: &Config, id: &str, theme: crate::theme::ThemeId) -> Result<()> {
    let store = ilar::runtime::session_store(config);
    store.load(id).with_context(|| format!("no session {id}"))?;
    let (mut terminal, _session) = crate::TerminalSession::start()?;
    let mut app = App::new();
    app.theme = theme;
    app.session_id = id.to_string();
    app.status = "read-only view".into();
    app.push_transcript_line(Line_::System(format!(
        "read-only view of session {id} — follows the file; q or Esc to leave"
    )));
    // The replay of a big session takes seconds; it runs on a worker
    // so the screen comes up at once, as the picker's restore does.
    // The tail opens on the same worker: it reads the whole file too.
    let mut tail: Option<SessionTail> = None;
    let mut pending: Option<Restore> = Some(spawn_restore(&store, id, true));
    let mut stale = false;
    loop {
        terminal.draw(|frame| app.render(frame))?;
        if pending.as_ref().is_some_and(|task| task.is_finished()) {
            let (view, opened) = pending.take().unwrap().await??;
            if let Some(opened) = opened {
                tail = Some(opened);
            }
            app.replace_transcript(view);
            if std::mem::take(&mut stale) {
                pending = Some(spawn_restore(&store, id, false));
            }
        }
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
        if let Some(tail) = tail.as_mut()
            && !tail.poll()?.is_empty()
        {
            // A change while a rebuild runs is picked up by one more
            // rebuild after it, not by a second one alongside.
            if pending.is_some() {
                stale = true;
            } else {
                pending = Some(spawn_restore(&store, id, false));
            }
        }
    }
    Ok(())
}

type Restore = tokio::task::JoinHandle<Result<(RestoredSessionView, Option<SessionTail>)>>;

/// The whole view again from the log, on a worker. A rebuild rather
/// than an append: the restore knows the folds, the rewinds and the
/// children, and a change is rare enough that re-reading the log is
/// fine. The first call opens the tail as well.
fn spawn_restore(store: &SessionStore, id: &str, open_tail: bool) -> Restore {
    let store = store.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        let reader = store.load(&id)?;
        let liveness = if driven(&store, &id) {
            Liveness::Running
        } else {
            Liveness::Settled
        };
        let view = restored_session_view_with_store(&reader, &store, liveness);
        let tail = if open_tail {
            Some(SessionTail::open(&store, &id).with_context(|| format!("following {id}"))?)
        } else {
            None
        };
        Ok((view, tail))
    })
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
