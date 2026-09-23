//! `ilar --view <session>`: a session on screen, read-only.
//!
//! The transcript is the TUI's own — the same restore the picker
//! does, the same renderer — but nothing drives the session: no
//! runtime, no writer lease, no provider. The file is followed, and
//! every change rebuilds the view, so a gateway chat or another TUI
//! can be watched while it works. The prompt says `read-only · q
//! leaves` and offers no send; anything but a scroll key is answered
//! with the one thing this view cannot do.

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
    let reader = store.load(id).with_context(|| format!("no session {id}"))?;
    // The footer names the session's model and directory, not ours.
    let model = reader.effective_model();
    let cwd = reader.meta().and_then(|meta| meta.cwd.clone());
    drop(reader);
    let (mut terminal, session) = crate::TerminalSession::start()?;
    // Not the TUI's greeting: Enter, Ctrl-J and Ctrl-P are all refused
    // here, and the line pushed below says what does work.
    let mut app = App::new().without_greeting();
    // The read-only prompt shows no footer and there is no help
    // overlay here, so nothing reads this today — but an App whose
    // terminal capabilities are a lie is a trap for whoever adds F1.
    app.keys.enhanced = session.keyboard_enhanced;
    app.theme = theme;
    app.session_id = id.to_string();
    app.read_only = true;
    app.status = "read-only view".into();
    app.current_model = model;
    if let Some(cwd) = cwd {
        app.cwd = cwd;
    }
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
            app.replace_transcript(view, Some(&store));
            if std::mem::take(&mut stale) {
                pending = Some(spawn_restore(&store, id, false));
            }
        }
        if crossterm::event::poll(POLL)? {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let control = key.modifiers.contains(KeyModifiers::CONTROL);
                    match view_key(key.code, control) {
                        ViewKey::Leave => break,
                        ViewKey::ScrollUp(Span::Line) => app.scroll_up(1),
                        ViewKey::ScrollDown(Span::Line) => app.scroll_down(1),
                        ViewKey::ScrollUp(Span::Page) => app.scroll_up(app.page_size()),
                        ViewKey::ScrollDown(Span::Page) => app.scroll_down(app.page_size()),
                        ViewKey::ToTop => app.scroll_to_top(),
                        ViewKey::ToTail => app.scroll_to_tail(),
                        ViewKey::Repaint => terminal.clear()?,
                        ViewKey::Refuse => app.set_notice(
                            "read-only view: open the session without --view to talk to it",
                            NoticeLevel::Warning,
                        ),
                        ViewKey::Help => app.set_notice(
                            "read-only view: ↑↓ PgUp/PgDn Home/End or the wheel scroll · Ctrl-L repaints · q or Esc leaves",
                            NoticeLevel::Info,
                        ),
                        ViewKey::Ignore => {}
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

/// How far a scroll key moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    Line,
    Page,
}

/// What a keystroke means here. Everything this view cannot do is one
/// answer — the refusal — rather than silence: a person who types into
/// `--view` has to learn something from the first key, not from Enter
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewKey {
    Leave,
    ScrollUp(Span),
    ScrollDown(Span),
    ToTop,
    ToTail,
    Repaint,
    Refuse,
    /// What the view's keys are, since it has no help overlay.
    Help,
    /// Not a keystroke anybody made: a bare modifier, or a key the
    /// terminal reported without a code.
    Ignore,
}

fn view_key(code: KeyCode, control: bool) -> ViewKey {
    match (code, control) {
        (KeyCode::Char('c'), true) | (KeyCode::Char('q'), false) | (KeyCode::Esc, false) => {
            ViewKey::Leave
        }
        (KeyCode::Up, _) => ViewKey::ScrollUp(Span::Line),
        (KeyCode::Down, _) => ViewKey::ScrollDown(Span::Line),
        (KeyCode::PageUp, _) => ViewKey::ScrollUp(Span::Page),
        (KeyCode::PageDown, _) => ViewKey::ScrollDown(Span::Page),
        (KeyCode::Home, _) => ViewKey::ToTop,
        (KeyCode::End, _) => ViewKey::ToTail,
        // The one root binding that still means something without a
        // runtime: a screen the terminal has scribbled over.
        (KeyCode::Char('l'), true) => ViewKey::Repaint,
        // F1 is where help lives everywhere else; here it is one line.
        (KeyCode::F(1), _) => ViewKey::Help,
        (KeyCode::Modifier(_) | KeyCode::Null, _) => ViewKey::Ignore,
        _ => ViewKey::Refuse,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Scrolling and leaving are the whole of what this view does;
    /// every other key is answered, not swallowed. The view used to
    /// answer Enter alone, so a person typing a message watched their
    /// letters disappear under an input box that looked live.
    #[test]
    fn every_key_but_a_scroll_is_answered() {
        for (code, control) in [
            (KeyCode::Char('q'), false),
            (KeyCode::Esc, false),
            (KeyCode::Char('c'), true),
        ] {
            assert_eq!(view_key(code, control), ViewKey::Leave, "{code:?}");
        }
        assert_eq!(view_key(KeyCode::Up, false), ViewKey::ScrollUp(Span::Line));
        assert_eq!(
            view_key(KeyCode::PageDown, false),
            ViewKey::ScrollDown(Span::Page)
        );
        assert_eq!(view_key(KeyCode::Home, false), ViewKey::ToTop);
        assert_eq!(view_key(KeyCode::End, false), ViewKey::ToTail);
        assert_eq!(view_key(KeyCode::Char('l'), true), ViewKey::Repaint);
        // F1 is where help lives everywhere else, and here it says so.
        assert_eq!(view_key(KeyCode::F(1), false), ViewKey::Help);
        for (code, control) in [
            (KeyCode::Char('h'), false),
            (KeyCode::Enter, false),
            (KeyCode::Char('f'), true),
            (KeyCode::Char('t'), true),
            (KeyCode::Tab, false),
            (KeyCode::Backspace, false),
        ] {
            assert_eq!(view_key(code, control), ViewKey::Refuse, "{code:?}");
        }
        assert_eq!(view_key(KeyCode::Null, false), ViewKey::Ignore);
    }

    /// The prompt says what it is. Nothing drives this session, so a
    /// send footer would be a promise the view cannot keep.
    #[test]
    fn the_read_only_prompt_offers_no_send() {
        // "Enter send ·" with the separator: the welcome line in the
        // transcript says "Enter sends, …" and is not the footer.
        let watching = screen(true, enhanced());
        assert!(watching.contains("read-only · q leaves"), "{watching}");
        assert!(!watching.contains("Enter send ·"), "{watching}");
        let live = screen(false, enhanced());
        assert!(live.contains("Enter send ·"), "{live}");
        assert!(!live.contains("read-only · q leaves"), "{live}");
    }

    /// A terminal whose handshake answered yes.
    fn enhanced() -> crate::input::TerminalKeys {
        crate::input::TerminalKeys {
            enhanced: true,
            modified_enter: false,
        }
    }

    fn screen(read_only: bool, keys: crate::input::TerminalKeys) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.read_only = read_only;
        app.keys = keys;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    /// A terminal that cannot report Shift-Enter is never offered it —
    /// pressing it there sends the draft, which reads as ilar swallowing
    /// a keystroke rather than as a key the terminal does not have. The
    /// welcome line is written before the terminal has been asked, so it
    /// names only the key that always works.
    #[test]
    fn the_prompt_offers_only_the_newline_keys_that_arrive() {
        let by_handshake = screen(false, enhanced());
        assert!(
            by_handshake.contains("Enter send · Shift-Enter/Ctrl-J newline"),
            "{by_handshake}"
        );
        // Including the welcome line, which is the first thing read and
        // is built before the terminal has been asked anything.
        assert!(
            !by_handshake.contains("Enter sends, Shift-Enter"),
            "the welcome line promises a key it cannot know about"
        );

        // The other route to being believed: tmux answers nothing to
        // the handshake and sends CSI 13;2u anyway, so a keystroke is
        // the only evidence there will be.
        let by_keystroke = screen(
            false,
            crate::input::TerminalKeys {
                enhanced: false,
                modified_enter: true,
            },
        );
        assert!(
            by_keystroke.contains("Enter send · Shift-Enter/Ctrl-J newline"),
            "{by_keystroke}"
        );

        let plain = screen(false, crate::input::TerminalKeys::default());
        assert!(plain.contains("Enter send · Ctrl-J newline"), "{plain}");
        assert!(
            !plain.contains("Shift-Enter"),
            "a key this terminal cannot send was offered: {plain}"
        );
    }
}
