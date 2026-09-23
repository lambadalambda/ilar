//! What the agent is doing right now, in one line: folded from the
//! loop's events for a status message the chat can watch, and the
//! board that keeps one such line per seat.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ilar::agent::LoopEvent;
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

/// A seat's line for as long as a turn runs on it. The updater makes
/// every call to the chat for it, in the order they were asked for: a
/// reply's hide waits out a post in flight, and a post never lands
/// above the reply that hid the line.
struct Line {
    channel: String,
    chat_id: String,
    /// Whether the status message is in the chat right now; `false`
    /// while a reply has taken it down and no work since put it back.
    shown: Arc<AtomicBool>,
    /// Ends with the id of the message still in the chat, if any.
    updater: tokio::task::JoinHandle<Option<String>>,
    /// Stops the updater between two of its calls.
    stop: tokio::sync::oneshot::Sender<()>,
    /// Handed to every turn that writes to this line, and to a reply
    /// that hides it.
    lines: mpsc::UnboundedSender<Order>,
    owner: u64,
}

/// What the updater is asked to do.
pub enum Order {
    Say(Update),
    /// Take the message out of the chat, then answer.
    Hide(tokio::sync::oneshot::Sender<()>),
}

/// One status line from the narrator. Work — a tool, a delegation, a
/// compaction — puts the line back up after a reply took it down; the
/// thinking and writing a turn ends with do not.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub text: String,
    pub work: bool,
}

impl Update {
    /// The narrator's line for `event`.
    pub fn of(text: String, event: &LoopEvent) -> Self {
        Self {
            text,
            work: is_work(event),
        }
    }
}

/// A turn's share of a chat's status line: where its lines go, and —
/// for the turn that took the line up, and only that one — the right
/// to take it down again.
pub struct Claim {
    key: String,
    owner: Option<u64>,
    lines: mpsc::UnboundedSender<Order>,
}

impl Claim {
    /// Pass the narrator's line on.
    pub fn say(&self, update: Update) {
        let _ = self.lines.send(Order::Say(update));
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
        let (tx, rx) = mpsc::unbounded_channel::<Order>();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let shown = Arc::new(AtomicBool::new(true));
        let mut updater = Some(tokio::spawn(edit_as_it_moves(
            channel.clone(),
            chat_id.to_string(),
            id.clone(),
            shown.clone(),
            self.interval,
            rx,
            stopped,
        )));
        let mut stop = Some(stop);
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
                            shown,
                            updater: updater.take().expect("only taken here"),
                            stop: stop.take().expect("only taken here"),
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
        // Ours has been sent nothing, so it has nothing in flight.
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

    /// Take a seat's line out of the chat whatever turn owns it: its
    /// reply is out, which is what the line was waiting for. The turn
    /// keeps its line, and its next work puts it back up under the
    /// reply.
    pub async fn clear(&self, key: &str) {
        let lines = self
            .lines
            .lock()
            .unwrap()
            .get(key)
            .map(|line| line.lines.clone());
        let Some(lines) = lines else {
            return;
        };
        let (done, hidden) = tokio::sync::oneshot::channel();
        if lines.send(Order::Hide(done)).is_ok() {
            let _ = hidden.await;
        }
    }

    async fn take_down(&self, key: &str, owner: Option<u64>) {
        let line = {
            let mut lines = self.lines.lock().unwrap();
            match lines.get(key) {
                Some(line) if owner.is_none_or(|owner| line.owner == owner) => lines.remove(key),
                _ => None,
            }
        };
        if let Some(line) = line {
            self.finish(key, line).await;
        }
    }

    /// Stop the updater, wait out its call in flight, then clear what
    /// it left in the chat — so a post landing now is cleared rather
    /// than left over a turn that has ended.
    async fn finish(&self, key: &str, line: Line) {
        let _ = line.stop.send(());
        let mut updater = line.updater;
        let shown = match tokio::time::timeout(STOP_GRACE, &mut updater).await {
            Ok(shown) => shown.ok().flatten(),
            Err(_) => {
                updater.abort();
                None
            }
        };
        if let Some(id) = shown
            && let Some(channel) = self.channels.get(&line.channel)
            && let Err(error) = channel.clear_status(&line.chat_id, &id).await
        {
            log(&format!("{key}: status not cleared: {error:#}"));
        }
    }

    /// Whether a seat has a line in the chat right now. A steer shows
    /// itself there; with no line, the chat needs telling another way.
    pub fn is_up(&self, key: &str) -> bool {
        self.lines
            .lock()
            .unwrap()
            .get(key)
            .is_some_and(|line| line.shown.load(Ordering::Acquire))
    }

    /// Take down every line there is, as the gateway goes down.
    ///
    /// A turn killed by `abort_all` never reaches the `end` that would
    /// have cleared its own line, so its "working…" bubble was left in
    /// the chat — still there on the next start, describing a turn that
    /// died with the process. Every seat's line, whoever owns it; all
    /// stopped at once, so one wedged call does not hold up the rest.
    pub async fn clear_all(&self) {
        let lines: Vec<(String, Line)> = self.lines.lock().unwrap().drain().collect();
        futures::future::join_all(
            lines
                .into_iter()
                .map(|(key, line)| async move { self.finish(&key, line).await }),
        )
        .await;
    }
}

/// How long taking a line down waits for its updater to finish a call.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Keep one line current: the newest line wins, and calls are no
/// closer together than the interval — on Delta Chat every edit is a
/// message on the wire. A reply's hide drops the line waiting from
/// before it; work after it posts a fresh line under it. Ends between
/// calls when stopped, with the id of the message it left in the chat.
async fn edit_as_it_moves(
    channel: Arc<dyn Channel>,
    chat_id: String,
    first: String,
    shown: Arc<AtomicBool>,
    interval: Duration,
    mut orders: mpsc::UnboundedReceiver<Order>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) -> Option<String> {
    let mut id = Some(first);
    let mut last_call: Option<tokio::time::Instant> = None;
    let mut said = WORKING.to_string();
    // The newest line, waiting out the interval, and whether any line
    // folded into it was work.
    let mut waiting: Option<Update> = None;
    loop {
        let due = waiting.as_ref().map(|_| match last_call {
            Some(last) => last + interval,
            None => tokio::time::Instant::now(),
        });
        let due = async {
            match due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => std::future::pending().await,
            }
        };
        // Once due, act on what is waiting: a line arriving in the same
        // instant waits its own interval, so every line shows when the
        // interval is zero.
        tokio::select! {
            biased;
            _ = &mut stop => return id,
            () = due => {}
            order = orders.recv() => {
                match order {
                    Some(Order::Say(update)) => {
                        let work = update.work || waiting.as_ref().is_some_and(|w| w.work);
                        waiting = Some(Update { work, ..update });
                        // Due already: act now, not a loop later, where
                        // a turn ending this instant would stop it first.
                        let due = last_call.is_none_or(|last| {
                            last + interval <= tokio::time::Instant::now()
                        });
                        if !due {
                            continue;
                        }
                    }
                    Some(Order::Hide(done)) => {
                        waiting = None;
                        if let Some(old) = id.take() {
                            shown.store(false, Ordering::Release);
                            if let Err(error) = channel.clear_status(&chat_id, &old).await {
                                log(&format!("status not cleared: {error:#}"));
                            }
                        }
                        let _ = done.send(());
                        // The reply is only queued yet: a line posted
                        // back waits an interval so it lands under it.
                        last_call = Some(tokio::time::Instant::now());
                        continue;
                    }
                    None => return id,
                }
            }
        }
        let Some(update) = waiting.take() else {
            continue;
        };
        match &id {
            Some(_) if update.text == said => continue,
            Some(current) => {
                if let Err(error) = channel.edit_status(&chat_id, current, &update.text).await {
                    log(&format!("status not edited: {error:#}"));
                }
            }
            None if update.work => match channel.post_status(&chat_id, &update.text).await {
                Ok(Some(posted)) => {
                    id = Some(posted);
                    shown.store(true, Ordering::Release);
                }
                Ok(None) => {}
                Err(error) => log(&format!("status not posted again: {error:#}")),
            },
            None => continue,
        }
        // A failed call waits its interval too, rather than retrying on
        // every line the narrator sends.
        said = update.text;
        last_call = Some(tokio::time::Instant::now());
    }
}

/// Whether an event is work a chat should see a line for even after a
/// reply took the line down: a tool, a delegation, a compaction, and
/// a reply that finished with another tool still running — not the
/// thinking and writing a turn ends with.
pub fn is_work(event: &LoopEvent) -> bool {
    match event {
        LoopEvent::ToolStarted { name, .. } => name != "message",
        LoopEvent::ToolFinished { name, .. } => name == "message",
        LoopEvent::ToolArguments { .. }
        | LoopEvent::ToolExecutionStarted { .. }
        | LoopEvent::SubagentConfigured { .. }
        | LoopEvent::Compacted { .. } => true,
        _ => false,
    }
}

const MAX_CHARS: usize = 90;

/// Folds events into the current status; says when it changed.
#[derive(Default)]
pub struct Narrator {
    tools: HashMap<String, String>,
    /// Each call's own line, for when it is running again after a reply.
    lines: HashMap<String, String>,
    /// Calls executing now, other than replies, in the order they began.
    executing: Vec<String>,
    summary: String,
    current: String,
}

impl Narrator {
    /// The new status line, when this event changed it — or, when a
    /// call is running under a reply that just took the line down, that
    /// call's line again.
    pub fn observe(&mut self, event: &LoopEvent) -> Option<String> {
        match event {
            // Streamed ahead of a reply in the same response, a call's
            // lines went with the line the reply took down; running is
            // when it is news again.
            LoopEvent::ToolExecutionStarted { id, .. } => {
                if self.tools.get(id).is_none_or(|name| name == "message") {
                    return None;
                }
                self.executing.push(id.clone());
                return Some(self.line_of(id));
            }
            LoopEvent::ToolFinished { id, name, .. } => {
                self.executing.retain(|running| running != id);
                self.lines.remove(id);
                let still = self.executing.first()?.clone();
                return (name == "message").then(|| self.line_of(&still));
            }
            _ => {}
        }
        let next = self.changed(event)?;
        if let LoopEvent::ToolArguments { id, .. } = event {
            self.lines.insert(id.clone(), next.clone());
        }
        Some(next)
    }

    fn line_of(&mut self, id: &str) -> String {
        let line = self.lines.get(id).cloned().unwrap_or_else(|| {
            let name = self.tools.get(id).map(String::as_str).unwrap_or_default();
            format!("running {name}")
        });
        self.current = line.clone();
        line
    }

    fn changed(&mut self, event: &LoopEvent) -> Option<String> {
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
                // about to go anyway, and whatever comes after it is
                // news to a chat that just saw it go.
                if name == "message" {
                    self.current.clear();
                    return None;
                }
                format!("calling {name}…")
            }
            // The loop's own summary, not the raw input parsed again:
            // `ToolArguments` carries what `summarize_tool_input`
            // made of the same call, and re-deriving it here meant
            // cloning an unbounded `write` body to produce a line.
            LoopEvent::ToolArguments { id, arguments } => {
                let name = self.tools.get(id).cloned().unwrap_or_default();
                if name == "message" {
                    return None;
                }
                if arguments.trim().is_empty() {
                    format!("running {name}")
                } else {
                    format!("running {name}: {arguments}")
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
        claim.say(work("running bash"));
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

    /// A reply mid-turn takes the line down, and the work after it put
    /// nothing back: minutes of tool calls looked like an idle agent
    /// until a message came back as a steer. Work puts a fresh line up
    /// under the reply; the last words of a turn do not.
    #[tokio::test]
    async fn work_after_a_reply_puts_a_line_back_up_under_it() {
        let (board, fake) = new_board(true);
        let claim = board.begin("fake:1", "fake", "1").await.expect("a line");
        board.clear("fake:1").await;
        assert!(!board.is_up("fake:1"));

        claim.say(Update {
            text: "writing…".into(),
            work: false,
        });
        claim.say(work("running bash: ls"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !board.is_up("fake:1") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the line back up");

        board.end(Some(claim)).await;
        assert!(!board.is_up("fake:1"));
        assert_eq!(
            fake.seen(),
            [
                Seen::StatusPosted(WORKING.into()),
                Seen::StatusCleared,
                Seen::StatusPosted("running bash: ls".into()),
                Seen::StatusCleared,
            ]
        );
    }

    /// A line still waiting out the interval when a reply hides the
    /// line is from before the reply: posted after it, it stood above
    /// the reply saying something already done.
    #[tokio::test]
    async fn a_reply_drops_the_line_waiting_from_before_it() {
        let fake = FakeChannel::new("fake");
        let channels = HashMap::from([("fake".to_string(), fake.clone() as Arc<dyn Channel>)]);
        let board = StatusBoard::new(channels, true, Duration::from_millis(100));
        let claim = board.begin("fake:1", "fake", "1").await.expect("a line");
        claim.say(work("running bash: a"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        claim.say(work("running bash: b"));
        board.clear("fake:1").await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!board.is_up("fake:1"));
        board.end(Some(claim)).await;
        assert_eq!(
            fake.seen(),
            [
                Seen::StatusPosted(WORKING.into()),
                Seen::StatusEdited("running bash: a".into()),
                Seen::StatusCleared,
            ]
        );
    }

    fn work(text: &str) -> Update {
        Update {
            text: text.into(),
            work: true,
        }
    }

    /// One response, a reply and a command: the command's lines stream
    /// before the reply runs and go down with it. It is news again
    /// when it runs — or, running already, when the reply is out.
    #[test]
    fn a_call_running_under_a_reply_gets_its_line_back() {
        let mut narrator = Narrator::default();
        let started = |id: &str, name: &str| LoopEvent::ToolStarted {
            id: id.into(),
            name: name.into(),
        };
        let running = |id: &str| LoopEvent::ToolExecutionStarted {
            id: id.into(),
            received_bytes: 0,
            started: std::time::Instant::now(),
        };
        let finished = |id: &str, name: &str| LoopEvent::ToolFinished {
            id: id.into(),
            name: name.into(),
            is_error: false,
            result: String::new(),
            child_session_id: None,
        };
        narrator.observe(&started("m", "message"));
        narrator.observe(&started("b", "bash"));
        let line = narrator
            .observe(&LoopEvent::ToolArguments {
                id: "b".into(),
                arguments: "cargo test".into(),
            })
            .unwrap();
        assert_eq!(narrator.observe(&running("m")), None);
        assert_eq!(narrator.observe(&running("b")), Some(line.clone()));
        let reply_done = finished("m", "message");
        assert_eq!(narrator.observe(&reply_done), Some(line));
        assert!(is_work(&reply_done));
        // Nothing left running: the reply's end is no news.
        assert_eq!(narrator.observe(&finished("b", "bash")), None);
        narrator.observe(&started("m2", "message"));
        assert_eq!(narrator.observe(&finished("m2", "message")), None);
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
        // The loop's own summary, which is what `ToolArguments`
        // carries; the raw-input event is a different one and is not
        // the narrator's to parse.
        let running = narrator
            .observe(&LoopEvent::ToolArguments {
                id: "1".into(),
                arguments: "cargo test -p ilar".into(),
            })
            .unwrap();
        assert!(running.starts_with("running bash: "), "{running}");
        assert!(running.contains("cargo test"), "{running}");
        assert_eq!(
            narrator.observe(&LoopEvent::ToolInputComplete {
                id: "1".into(),
                arguments: r#"{"command": "cargo test -p ilar"}"#.into(),
            }),
            None,
            "the narrator re-derived the summary from the raw input"
        );
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
        // After a reply the same words are news again: the line they
        // were on went with it.
        narrator.observe(&LoopEvent::ToolStarted {
            id: "3".into(),
            name: "message".into(),
        });
        assert_eq!(
            narrator.observe(&LoopEvent::TextDelta("z".into())),
            Some("writing…".into())
        );
        let long = "a".repeat(300);
        let clipped = narrator
            .observe(&LoopEvent::ReasoningSummaryDelta(format!("**{long}**\n")))
            .unwrap();
        assert!(clipped.chars().count() <= MAX_CHARS, "{clipped}");
        assert!(clipped.ends_with('…'));
    }
}
