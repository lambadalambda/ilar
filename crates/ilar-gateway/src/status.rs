//! What the agent is doing right now, in one line: folded from the
//! loop's events for a status message the chat can watch, and the
//! board that keeps one such line per seat.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ilar::agent::{LoopEvent, summarize_tool_input};
use tokio::sync::mpsc;

use crate::channel::Channel;
use crate::driver::log;

/// The line shown before anything has happened.
pub const WORKING: &str = "working…";

/// The status lines the chats are watching, one per seat, and the
/// power to take one down. Shared with the message tool: a turn's own
/// reply to its own chat is what that chat's line was waiting for.
pub struct StatusBoard {
    channels: HashMap<String, Arc<dyn Channel>>,
    lines: Mutex<HashMap<String, Line>>,
    /// Hands out the token that says which turn owns a line.
    claims: AtomicU64,
    /// Off: nothing is ever posted, and `is_up` is always false.
    enabled: bool,
    /// The least time between two edits of a line.
    interval: Duration,
}

/// A posted line, the task that keeps it current, and the turn it
/// belongs to.
struct Line {
    channel: String,
    chat_id: String,
    id: String,
    updater: tokio::task::JoinHandle<()>,
    /// Handed to every turn that writes to this line.
    lines: mpsc::UnboundedSender<String>,
    owner: u64,
}

/// A turn's share of a chat's status line: where its lines go, and —
/// for the turn that took the line up, and only that one — the right
/// to take it down again.
pub struct Claim {
    key: String,
    owner: Option<u64>,
    lines: mpsc::UnboundedSender<String>,
}

impl Claim {
    /// Where the narrator's lines go.
    pub fn lines(&self) -> mpsc::UnboundedSender<String> {
        self.lines.clone()
    }
}

impl StatusBoard {
    pub fn new(
        channels: HashMap<String, Arc<dyn Channel>>,
        enabled: bool,
        interval: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels,
            lines: Mutex::new(HashMap::new()),
            claims: AtomicU64::new(0),
            enabled,
            interval,
        })
    }

    /// A line for the turn about to run on `key`: "working…", then
    /// whatever the narrator says, edited no more often than the
    /// interval allows. A line already up is shared, never replaced —
    /// the turn that posted it is still running, and its bubble must
    /// not be left in the chat with nobody to clear it. `None` when
    /// the board is off, the channel has no such thing, or posting
    /// failed.
    pub async fn begin(&self, key: &str, channel_name: &str, chat_id: &str) -> Option<Claim> {
        if !self.enabled {
            return None;
        }
        if let Some(shared) = self.share(key) {
            return Some(shared);
        }
        let channel = self.channels.get(channel_name)?.clone();
        let id = match channel.post_status(chat_id, WORKING).await {
            Ok(Some(id)) => id,
            Ok(None) => return None,
            Err(error) => {
                log(&format!("{key}: status not posted: {error:#}"));
                return None;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let mut updater = Some(tokio::spawn(edit_as_it_moves(
            channel.clone(),
            chat_id.to_string(),
            id.clone(),
            self.interval,
            rx,
        )));
        let owner = self.claims.fetch_add(1, Ordering::AcqRel);
        let shared = {
            let mut lines = self.lines.lock().unwrap();
            match lines.get(key) {
                Some(line) => Some(line.lines.clone()),
                None => {
                    lines.insert(
                        key.to_string(),
                        Line {
                            channel: channel_name.to_string(),
                            chat_id: chat_id.to_string(),
                            id: id.clone(),
                            updater: updater.take().expect("only taken here"),
                            lines: tx.clone(),
                            owner,
                        },
                    );
                    None
                }
            }
        };
        // Another turn on the same seat posted one while we awaited:
        // theirs stands and ours goes, rather than two lines standing.
        if let Some(shared) = shared {
            if let Some(updater) = updater {
                updater.abort();
            }
            if let Err(error) = channel.clear_status(chat_id, &id).await {
                log(&format!("{key}: status not cleared: {error:#}"));
            }
            return Some(Claim {
                key: key.to_string(),
                owner: None,
                lines: shared,
            });
        }
        Some(Claim {
            key: key.to_string(),
            owner: Some(owner),
            lines: tx,
        })
    }

    /// A share of the line already up on `key`, if there is one.
    fn share(&self, key: &str) -> Option<Claim> {
        let lines = self.lines.lock().unwrap();
        let line = lines.get(key)?;
        Some(Claim {
            key: key.to_string(),
            owner: None,
            lines: line.lines.clone(),
        })
    }

    /// The turn that took the line up takes it down as it ends; a turn
    /// that only shared it leaves it standing for its owner.
    pub async fn end(&self, claim: Option<Claim>) {
        let Some(claim) = claim else {
            return;
        };
        let Some(owner) = claim.owner else {
            return;
        };
        self.take_down(&claim.key, Some(owner)).await;
    }

    /// Take a seat's line down whatever turn owns it: its reply is out,
    /// which is what the line was waiting for.
    pub async fn clear(&self, key: &str) {
        self.take_down(key, None).await;
    }

    async fn take_down(&self, key: &str, owner: Option<u64>) {
        let line = {
            let mut lines = self.lines.lock().unwrap();
            match lines.get(key) {
                Some(line) if owner.is_none_or(|owner| line.owner == owner) => lines.remove(key),
                _ => None,
            }
        };
        let Some(line) = line else {
            return;
        };
        line.updater.abort();
        if let Some(channel) = self.channels.get(&line.channel)
            && let Err(error) = channel.clear_status(&line.chat_id, &line.id).await
        {
            log(&format!("{key}: status not cleared: {error:#}"));
        }
    }

    /// Whether a seat has a line up right now. A steer shows itself
    /// there; with no line, the chat needs telling another way.
    pub fn is_up(&self, key: &str) -> bool {
        self.lines.lock().unwrap().contains_key(key)
    }
}

/// Keep one posted line current: the newest line wins, and edits are
/// no closer together than the interval — on Delta Chat every edit is
/// a message on the wire.
async fn edit_as_it_moves(
    channel: Arc<dyn Channel>,
    chat_id: String,
    status_id: String,
    interval: Duration,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    let mut last_edit: Option<tokio::time::Instant> = None;
    let mut shown = WORKING.to_string();
    while let Some(mut line) = rx.recv().await {
        // Wait out the interval, keeping only the newest line.
        if let Some(last) = last_edit {
            let due = last + interval;
            loop {
                // Once due, post what we have: a line arriving in the
                // same instant waits its own interval, so every line
                // shows when the interval is zero.
                tokio::select! {
                    biased;
                    () = tokio::time::sleep_until(due) => break,
                    newer = rx.recv() => match newer {
                        Some(newer) => line = newer,
                        None => return,
                    },
                }
            }
        }
        if line == shown {
            continue;
        }
        if let Err(error) = channel.edit_status(&chat_id, &status_id, &line).await {
            log(&format!("status not edited: {error:#}"));
            return;
        }
        shown = line;
        last_edit = Some(tokio::time::Instant::now());
    }
}

const MAX_CHARS: usize = 90;

/// Folds events into the current status; says when it changed.
#[derive(Default)]
pub struct Narrator {
    tools: HashMap<String, String>,
    summary: String,
    current: String,
}

impl Narrator {
    /// The new status line, when this event changed it.
    pub fn observe(&mut self, event: &LoopEvent) -> Option<String> {
        let next = match event {
            LoopEvent::ReasoningSummaryDelta(delta) => {
                self.summary.push_str(delta);
                match heading(&self.summary) {
                    Some(topic) => format!("thinking — {topic}"),
                    None => "thinking…".to_string(),
                }
            }
            LoopEvent::ReasoningSummaryCompleted => {
                self.summary.clear();
                return None;
            }
            LoopEvent::ToolStarted { id, name } => {
                self.tools.insert(id.clone(), name.clone());
                // The reply on its way is not a status; the line is
                // about to go anyway.
                if name == "message" {
                    return None;
                }
                format!("calling {name}…")
            }
            LoopEvent::ToolInputComplete { id, arguments } => {
                let name = self.tools.get(id).cloned().unwrap_or_default();
                if name == "message" {
                    return None;
                }
                let input: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
                let summary = summarize_tool_input(&name, &input);
                if summary.trim().is_empty() {
                    format!("running {name}")
                } else {
                    format!("running {name}: {summary}")
                }
            }
            LoopEvent::SubagentConfigured {
                description, agent, ..
            } => format!("delegating to {agent}: {description}"),
            LoopEvent::Steered { text, .. } => format!("steered: {}", clip(text)),
            LoopEvent::TextDelta(_) => "writing…".to_string(),
            LoopEvent::Compacted { .. } => "compacting the conversation…".to_string(),
            LoopEvent::ProviderRetry {
                attempt,
                max_retries,
                ..
            } => format!("the provider stumbled; retry {attempt} of {max_retries}…"),
            _ => return None,
        };
        let next = clip(&next);
        if next == self.current {
            return None;
        }
        self.current = next.clone();
        Some(next)
    }
}

/// The bold heading a reasoning summary opens with, or its first line.
fn heading(summary: &str) -> Option<String> {
    let text = summary.trim_start();
    if let Some(rest) = text.strip_prefix("**")
        && let Some(end) = rest.find("**")
    {
        let topic = rest[..end].trim();
        return (!topic.is_empty()).then(|| topic.to_string());
    }
    let line = text.lines().next()?.trim().trim_matches('*').trim();
    (!line.is_empty() && text.contains('\n')).then(|| line.to_string())
}

fn clip(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{FakeChannel, Seen};

    /// A board over one fake channel, and the channel to read.
    fn new_board(enabled: bool) -> (Arc<StatusBoard>, Arc<FakeChannel>) {
        let fake = FakeChannel::new("fake");
        let channels = HashMap::from([("fake".to_string(), fake.clone() as Arc<dyn Channel>)]);
        (
            StatusBoard::new(channels, enabled, Duration::ZERO),
            fake.clone(),
        )
    }

    #[tokio::test]
    async fn one_line_per_seat_and_only_its_owner_takes_it_down() {
        let (board, fake) = new_board(true);
        let first = board.begin("fake:1", "fake", "1").await.expect("a line");
        // A second turn on the same seat — a subagent's report while
        // the person's turn runs — shares the line instead of leaving
        // the first one in the chat with nobody to clear it.
        let shared = board.begin("fake:1", "fake", "1").await.expect("a share");
        assert_eq!(fake.seen(), [Seen::StatusPosted(WORKING.into())]);
        assert!(board.is_up("fake:1"));
        board.end(Some(shared)).await;
        assert!(board.is_up("fake:1"), "a sharer took the line down");
        assert_eq!(fake.seen().len(), 1);
        // Its owner does, once.
        board.end(Some(first)).await;
        assert!(!board.is_up("fake:1"));
        board.clear("fake:1").await;
        assert_eq!(
            fake.seen(),
            [Seen::StatusPosted(WORKING.into()), Seen::StatusCleared]
        );
        // The next turn on the seat gets a line of its own: a turn
        // that waited its way through the seat is not left writing
        // into the line that came down with the last reply.
        let next = board.begin("fake:1", "fake", "1").await.expect("a line");
        assert!(board.is_up("fake:1"));
        board.end(Some(next)).await;
        assert_eq!(
            fake.seen()
                .iter()
                .filter(|s| matches!(s, Seen::StatusPosted(_)))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_reply_clears_the_line_whoever_owns_it_and_an_off_board_posts_nothing() {
        let (board, fake) = new_board(true);
        let claim = board.begin("fake:1", "fake", "1").await.expect("a line");
        claim.lines().send("running bash".into()).unwrap();
        // The chat's reply is what the line was waiting for, and the
        // turn that owns it finds nothing left to take down.
        board.clear("fake:1").await;
        board.end(Some(claim)).await;
        assert_eq!(
            fake.seen()
                .iter()
                .filter(|s| **s == Seen::StatusCleared)
                .count(),
            1,
            "{:?}",
            fake.seen()
        );

        let (off, quiet) = new_board(false);
        assert!(off.begin("fake:1", "fake", "1").await.is_none());
        assert!(!off.is_up("fake:1"));
        off.clear("fake:1").await;
        assert!(quiet.seen().is_empty());
    }

    #[test]
    fn events_become_a_status_line_that_only_reports_changes() {
        let mut narrator = Narrator::default();
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta("**Planning".into())),
            Some("thinking…".into())
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta(
                " the fix**\n\nFirst".into()
            )),
            Some("thinking — Planning the fix".into())
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryDelta(" more".into())),
            None
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ReasoningSummaryCompleted),
            None
        );
        assert_eq!(
            narrator.observe(&LoopEvent::ToolStarted {
                id: "1".into(),
                name: "bash".into()
            }),
            Some("calling bash…".into())
        );
        let running = narrator
            .observe(&LoopEvent::ToolInputComplete {
                id: "1".into(),
                arguments: r#"{"command": "cargo test -p ilar"}"#.into(),
            })
            .unwrap();
        assert!(running.starts_with("running bash: "), "{running}");
        assert!(running.contains("cargo test"), "{running}");
        assert_eq!(narrator.observe(&LoopEvent::TurnStarted), None);
        assert_eq!(
            narrator.observe(&LoopEvent::ToolStarted {
                id: "2".into(),
                name: "message".into()
            }),
            None
        );
        assert_eq!(
            narrator.observe(&LoopEvent::TextDelta("x".into())),
            Some("writing…".into())
        );
        assert_eq!(narrator.observe(&LoopEvent::TextDelta("y".into())), None);
        let long = "a".repeat(300);
        let clipped = narrator
            .observe(&LoopEvent::ReasoningSummaryDelta(format!("**{long}**\n")))
            .unwrap();
        assert!(clipped.chars().count() <= MAX_CHARS, "{clipped}");
        assert!(clipped.ends_with('…'));
    }
}
