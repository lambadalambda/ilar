//! A tool's ask for a secret, answered from the chat. Nobody sits at a
//! channel to fill in a form, but a yes or no fits in a message: the
//! ask is posted to the chat the seat belongs to, and `/grant` or
//! `/deny` answers it. Unanswered long enough, it is a no.
//!
//! sudo's password is a second ask of the same kind, put after the yes
//! and only where sudo wants one; `/password <pw>` answers that, and
//! the message it came in is deleted the way `/unlock`'s is.

use std::sync::{Arc, Mutex};

use ilar::secrets::{Ask, AskReceiver, Grant, GrantPrompt, PasswordPrompt, UnlockPrompt};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::bus::Outbound;

/// How long an ask waits for the chat before it is refused. The same
/// for either ask: a password nobody types is a no as much as a grant
/// nobody gives.
pub const GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// What the chat said. The commands build these; the pending ask below
/// says which of them it can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// `/grant [once|session|always]`.
    Grant(Grant),
    /// `/password <pw>`.
    Password(String),
    /// `/deny`, and what a timeout comes to.
    No,
}

/// The ask the chat has not answered yet.
pub struct PendingGrant {
    pub secret: String,
    pub asker: Asker,
    /// This is sudo's password ask, not a grant ask: `/password` answers
    /// it and `/grant` is told so, rather than a password being held as
    /// an approval or the other way round.
    pub password: bool,
    answer: oneshot::Sender<Answer>,
}

/// Who asked: what the chat is shown, and the bare tool name a CLI
/// line needs. A subagent's ask is marked — the person did not ask for
/// that command themselves — but `ilar secret revoke --tool` wants the
/// tool, not the mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asker {
    pub shown: String,
    pub tool: String,
}

impl Asker {
    /// The prompt's asker, judged against the seat's own session: an
    /// ask from another session came from a child of it.
    pub fn of(prompt: &GrantPrompt, session_id: &str) -> Self {
        Self::named(
            &prompt.tool,
            prompt.session_id == session_id,
            prompt.agent.as_deref(),
        )
    }

    /// The same for the password ask, which is always sudo's.
    pub fn of_password(prompt: &PasswordPrompt, session_id: &str) -> Self {
        Self::named(
            "sudo",
            prompt.session_id == session_id,
            prompt.agent.as_deref(),
        )
    }

    /// The same for the unlock ask, which names the tool held up by the
    /// sealed store.
    pub fn of_unlock(prompt: &UnlockPrompt, session_id: &str) -> Self {
        Self::named(
            &prompt.tool,
            prompt.session_id == session_id,
            prompt.agent.as_deref(),
        )
    }

    fn named(tool: &str, own_session: bool, agent: Option<&str>) -> Self {
        // One rule, in the core: this had its own copy here with the
        // boolean the other way round.
        let shown = ilar::secrets::asker_label(tool, !own_session, agent);
        Self {
            shown,
            tool: tool.to_string(),
        }
    }
}

/// One seat's slot for the ask in flight: a tool blocks on its answer,
/// so there is never more than one.
pub type PendingSlot = Arc<Mutex<Option<PendingGrant>>>;

/// What was decided, for the chat.
pub fn decided(secret: &str, asker: &Asker, grant: Option<Grant>) -> String {
    let shown = &asker.shown;
    match grant {
        Some(Grant::Once) => format!("{secret} allowed for {shown}, this once."),
        Some(Grant::Session) => {
            format!("{secret} allowed for {shown} until this chat is restarted.")
        }
        Some(Grant::Always) => format!(
            "{secret} allowed for {shown} from now on; ilar secret revoke {secret} --tool {} \
             takes it back.",
            asker.tool
        ),
        None => format!("{secret} denied for {shown}."),
    }
}

/// The verdict when the chat let the ask time out.
pub fn no_answer(secret: &str, asker: &Asker) -> String {
    format!("{secret} denied for {} (no answer).", asker.shown)
}

/// Answer the ask waiting on `slot`, if any. `Err` says why nothing
/// was answered; the ask then stands, so the right command still
/// reaches it.
pub fn answer(slot: &PendingSlot, answer: Answer) -> Result<String, &'static str> {
    // One guard for the whole decision: an ask the command cannot
    // answer is left where it is rather than taken out and put back,
    // which could drop an ask that arrived in between.
    let mut held = slot.lock().unwrap();
    let Some(pending) = held.as_ref() else {
        return Err("Nothing is waiting for a grant.");
    };
    // The wrong command for the ask in flight: say which question is
    // waiting, and leave it waiting.
    match (pending.password, &answer) {
        (true, Answer::Grant(_)) => {
            return Err(
                "That ask is for the sudo password, not the approval: /password <pw>, or /deny.",
            );
        }
        (false, Answer::Password(_)) => {
            return Err(
                "That ask is the approval question: /grant, /grant session or /grant always, \
                 or /deny. The password is asked for after the yes.",
            );
        }
        _ => {}
    }
    let pending = held.take().expect("checked just above");
    drop(held);
    let text = match &answer {
        Answer::Password(_) => format!("Password sent to {}.", pending.asker.shown),
        Answer::No if pending.password => format!(
            "No password given; that {} command does not run.",
            pending.asker.shown
        ),
        Answer::Grant(grant) => decided(&pending.secret, &pending.asker, Some(*grant)),
        Answer::No => decided(&pending.secret, &pending.asker, None),
    };
    pending
        .answer
        .send(answer)
        .map(|()| text)
        .map_err(|_| "The tool stopped waiting for that.")
}

/// The most of a command one ask shows, counted the way Delta Chat
/// folds a bubble: a display line is 100 characters or a line break,
/// and past 34 of them the message is cut behind "show full message".
/// The ask's own words take a handful, so the command gets these, and
/// a tail naming what is not shown — better than the header, the
/// command and the instructions arriving as separate bubbles.
const ASK_COMMAND_LINES: usize = 20;
const ASK_LINE_CHARS: usize = 100;
/// The indent every line of the command wears.
const ASK_INDENT: &str = "    ";

/// How many display lines a line of the command takes, indent and all.
fn display_lines(line: &str) -> usize {
    (ASK_INDENT.len() + line.chars().count())
        .max(1)
        .div_ceil(ASK_LINE_CHARS)
}

/// The command as the ask shows it: verbatim, every line indented, so
/// its last line cannot be read as part of the instructions, and
/// clipped with a tail when it is longer than one message can hold.
fn shown_command(detail: &str) -> String {
    if detail.trim().is_empty() {
        return format!("{ASK_INDENT}(no command)\n");
    }
    let lines: Vec<&str> = detail.lines().collect();
    let mut shown = String::new();
    let mut used = 0;
    let mut whole = 0;
    for line in &lines {
        let needed = display_lines(line);
        if used + needed > ASK_COMMAND_LINES {
            // Cut to what the lines left over hold, and marked as cut
            // where the cut is.
            let room = (ASK_COMMAND_LINES - used) * ASK_LINE_CHARS;
            if room > ASK_INDENT.len() + 1 {
                let cut: String = line.chars().take(room - ASK_INDENT.len() - 1).collect();
                shown.push_str(&format!("{ASK_INDENT}{cut}…\n"));
                whole += 1;
            }
            break;
        }
        shown.push_str(&format!("{ASK_INDENT}{line}\n"));
        used += needed;
        whole += 1;
    }
    let hidden = lines.len() - whole;
    if hidden > 0 {
        shown.push_str(&format!(
            "{ASK_INDENT}… {hidden} more line{} not shown\n",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    shown
}

/// The message the chat gets for a grant ask.
pub fn ask_text(prompt: &GrantPrompt, asker: &Asker) -> String {
    let purpose = if prompt.description.is_empty() {
        String::new()
    } else {
        format!(" ({})", prompt.description)
    };
    format!(
        "🔑 {} wants {}{purpose} to run:\n\n{}\n/grant allows it this once, /grant session or \
         /grant always for longer, /deny refuses. The turn waits for your answer; unanswered in \
         {} minutes, it is a no.",
        asker.shown,
        prompt.secret,
        shown_command(&prompt.detail),
        GRANT_TIMEOUT.as_secs() / 60
    )
}

/// The message the chat gets when a call wants a store that is sealed.
/// Not a question: the master password belongs in `/unlock`, which the
/// adapter takes back out of the room's history where it can, and a
/// reply to a prompt is an ordinary message that stays there. So the
/// ask goes unanswered — the store stays shut, as it was before there
/// was an ask at all — and the room is told what opens it.
pub fn unlock_ask_text(prompt: &UnlockPrompt, asker: &Asker) -> String {
    format!(
        "🔒 {} wants a stored secret and the store is sealed:\n\n{}\n{} to open it for as long as \
         this gateway runs. Until then the call is refused.",
        asker.shown,
        shown_command(&prompt.detail),
        crate::commands::UNLOCK_HINT
    )
}

/// The message the chat gets when sudo wants a password for a command
/// it has already been allowed to run.
pub fn password_ask_text(prompt: &PasswordPrompt, asker: &Asker) -> String {
    let again = if prompt.refused {
        "sudo refused the last password. "
    } else {
        ""
    };
    format!(
        "🔑 {again}{} needs the sudo password to run:\n\n{}\n/password <pw> answers it; the \
         message is deleted afterwards where the channel allows it, and the password is held in \
         memory until this chat is restarted. /deny refuses, and so does {} minutes without an \
         answer.",
        asker.shown,
        shown_command(&prompt.detail),
        GRANT_TIMEOUT.as_secs() / 60
    )
}

/// The seat an ask belongs to: where it is posted, and whose session
/// it is — what tells the seat's own asks from its children's.
#[derive(Debug, Clone)]
pub struct Home {
    pub channel: String,
    pub chat_id: String,
    pub session_id: String,
}

/// The reply path of whichever ask is in flight: the two carry
/// different answers, and only the command that matches the ask gets
/// through [`answer`].
enum Reply {
    Grant(oneshot::Sender<Option<Grant>>),
    Password(oneshot::Sender<Option<String>>),
}

impl Reply {
    /// Resolves when the tool stops waiting: the turn ended under the
    /// ask, so the slot is nobody's to answer any more.
    async fn closed(&mut self) {
        match self {
            Reply::Grant(reply) => reply.closed().await,
            Reply::Password(reply) => reply.closed().await,
        }
    }

    /// Hand the chat's answer to the tool. A mismatch cannot arrive —
    /// [`answer`] refuses the wrong command for the ask — and is a
    /// refusal here rather than a panic in a gateway.
    fn send(self, answer: Answer) {
        match (self, answer) {
            (Reply::Grant(reply), Answer::Grant(grant)) => {
                let _ = reply.send(Some(grant));
            }
            (Reply::Password(reply), Answer::Password(password)) => {
                let _ = reply.send(Some(password));
            }
            (Reply::Grant(reply), _) => {
                let _ = reply.send(None);
            }
            (Reply::Password(reply), _) => {
                let _ = reply.send(None);
            }
        }
    }

    fn deny(self) {
        self.send(Answer::No);
    }
}

/// Post every ask to the chat and relay the chat's answer. Ends with
/// the runtime (the receiver closes) or the gateway (`cancel`).
pub async fn watch(
    mut prompts: AskReceiver,
    slot: PendingSlot,
    outbound: mpsc::Sender<Outbound>,
    home: Home,
    timeout: std::time::Duration,
    cancel: CancellationToken,
) {
    let Home {
        channel,
        chat_id,
        session_id,
    } = home;
    loop {
        let ask = tokio::select! {
            ask = prompts.recv() => match ask {
                Some(ask) => ask,
                None => return,
            },
            _ = cancel.cancelled() => return,
        };
        let post = |text: String| {
            let outbound = outbound.clone();
            let channel = channel.clone();
            let chat_id = chat_id.clone();
            async move {
                let _ = outbound
                    .send(Outbound {
                        channel,
                        chat_id,
                        text,
                        media: Vec::new(),
                    })
                    .await;
            }
        };
        // Each ask, told apart once: the chat's text, what the slot
        // says it takes, and where the answer goes.
        let (text, pending_secret, password, asker, mut reply) = match ask {
            Ask::Grant(prompt) => {
                let asker = Asker::of(&prompt, &session_id);
                (
                    ask_text(&prompt, &asker),
                    prompt.secret,
                    false,
                    asker,
                    Reply::Grant(prompt.reply),
                )
            }
            Ask::Password(prompt) => {
                let asker = Asker::of_password(&prompt, &session_id);
                (
                    password_ask_text(&prompt, &asker),
                    ilar::secrets::SUDO_PASSWORD.to_string(),
                    true,
                    asker,
                    Reply::Password(prompt.reply),
                )
            }
            // The one ask this chat does not answer: see
            // [`unlock_ask_text`]. Saying so is the whole handling, and
            // the ask is dropped, which leaves the store shut.
            Ask::Unlock(prompt) => {
                let asker = Asker::of_unlock(&prompt, &session_id);
                post(unlock_ask_text(&prompt, &asker)).await;
                continue;
            }
        };
        post(text).await;
        let (answer_tx, answer_rx) = oneshot::channel();
        *slot.lock().unwrap() = Some(PendingGrant {
            secret: pending_secret.clone(),
            asker: asker.clone(),
            password,
            answer: answer_tx,
        });
        tokio::select! {
            answer = answer_rx => {
                // The command took the slot and decided; the tool gets
                // what the chat said, and a chat that already moved on
                // (the sender dropped unanswered) gets a no.
                match answer {
                    Ok(answer) => reply.send(answer),
                    Err(_) => reply.deny(),
                }
            }
            _ = reply.closed() => {
                // The turn ended (aborted, or the gateway is stopping)
                // while the ask stood: the slot is stale, not the
                // person's to answer any more.
                slot.lock().unwrap().take();
                post("That ask is over: the tool stopped waiting.".into()).await;
            }
            _ = tokio::time::sleep(timeout) => {
                slot.lock().unwrap().take();
                reply.deny();
                post(no_answer(&pending_secret, &asker)).await;
            }
            _ = cancel.cancelled() => {
                slot.lock().unwrap().take();
                reply.deny();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(reply: oneshot::Sender<Option<Grant>>) -> GrantPrompt {
        GrantPrompt {
            session_id: "s".into(),
            agent: None,
            tool_call_id: None,
            tool: "bash".into(),
            secret: "GITHUB_TOKEN".into(),
            description: "for gh".into(),
            detail: "gh pr list".into(),
            reply,
        }
    }

    fn password_prompt(reply: oneshot::Sender<Option<String>>) -> PasswordPrompt {
        PasswordPrompt {
            session_id: "s".into(),
            tool_call_id: None,
            agent: None,
            detail: "apt install ripgrep".into(),
            refused: false,
            reply,
        }
    }

    fn unlock_prompt(reply: oneshot::Sender<Option<String>>) -> UnlockPrompt {
        UnlockPrompt {
            session_id: "s".into(),
            agent: None,
            tool_call_id: None,
            tool: "bash".into(),
            detail: "gh pr list".into(),
            refused: false,
            reply,
        }
    }

    struct Harness {
        prompts: mpsc::Sender<Ask>,
        slot: PendingSlot,
        outbound: mpsc::Receiver<Outbound>,
        cancel: CancellationToken,
    }

    impl Harness {
        /// Wait for the ask to be posted and the slot to be filled: the
        /// commands read the slot, not the channel.
        async fn asked(&mut self) -> Outbound {
            let posted = self.outbound.recv().await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.slot.lock().unwrap().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            posted
        }
    }

    fn harness(timeout: std::time::Duration) -> Harness {
        let (prompts, receiver) = ilar::secrets::ask_channel(1);
        let slot: PendingSlot = Arc::new(Mutex::new(None));
        let (outbound_tx, outbound) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        tokio::spawn(watch(
            receiver,
            slot.clone(),
            outbound_tx,
            Home {
                channel: "deltachat".into(),
                chat_id: "12".into(),
                session_id: "s".into(),
            },
            timeout,
            cancel.clone(),
        ));
        Harness {
            prompts,
            slot,
            outbound,
            cancel,
        }
    }

    /// The sealed store is the ask this chat does not take: the room is
    /// told what wants it and what opens it, nothing goes in the slot
    /// for `/grant` or `/password` to answer, and the call is left
    /// refused — the password belongs in `/unlock`, which the adapter
    /// can take back out of the history.
    #[tokio::test]
    async fn the_unlock_ask_points_at_the_command_and_answers_nothing() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts
            .send(Ask::Unlock(unlock_prompt(reply)))
            .await
            .unwrap();
        let posted = h.outbound.recv().await.unwrap();
        assert!(
            posted.text.contains("the store is sealed"),
            "{}",
            posted.text
        );
        assert!(posted.text.contains("gh pr list"), "{}", posted.text);
        assert!(posted.text.contains("/unlock"), "{}", posted.text);
        assert!(!posted.text.contains("/grant"), "{}", posted.text);
        assert!(receive.await.is_err(), "the chat answered an unlock ask");
        assert!(h.slot.lock().unwrap().is_none(), "it took the ask slot");
        // And the next ask is served as usual: nothing was left behind.
        let (reply, _receive) = oneshot::channel();
        h.prompts.send(Ask::Grant(prompt(reply))).await.unwrap();
        assert!(h.asked().await.text.contains("/grant"));
        h.cancel.cancel();
    }

    #[tokio::test]
    async fn the_ask_is_posted_and_the_chats_answer_reaches_the_tool() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts.send(Ask::Grant(prompt(reply))).await.unwrap();
        let posted = h.asked().await;
        assert_eq!(
            (posted.channel.as_str(), posted.chat_id.as_str()),
            ("deltachat", "12")
        );
        // The command stands apart: a blank line and an indent, so its
        // last line is never read as part of the instructions.
        assert!(
            posted
                .text
                .contains("bash wants GITHUB_TOKEN (for gh) to run:\n\n    gh pr list\n\n/grant"),
            "{}",
            posted.text
        );
        assert!(!posted.text.contains("/password"), "{}", posted.text);
        // A password sent to the approval question is refused and the
        // ask stands: it would otherwise be lost, and the turn would
        // still be waiting for a yes.
        let refused = answer(&h.slot, Answer::Password("typo".into()));
        assert!(
            refused.unwrap_err().contains("the approval question"),
            "the approval ask took a password"
        );
        let text = answer(&h.slot, Answer::Grant(Grant::Session)).unwrap();
        assert!(
            text.starts_with("GITHUB_TOKEN allowed for bash until"),
            "{text}"
        );
        assert_eq!(receive.await.unwrap(), Some(Grant::Session));
        assert_eq!(
            answer(&h.slot, Answer::No),
            Err("Nothing is waiting for a grant.")
        );
        h.cancel.cancel();
    }

    /// The password ask is the same protocol: posted to the chat, named
    /// as sudo's, answered by `/password` — and `/grant` on it is told
    /// what it is looking at rather than eaten.
    #[tokio::test]
    async fn the_password_ask_is_answered_by_password_and_not_by_grant() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts
            .send(Ask::Password(password_prompt(reply)))
            .await
            .unwrap();
        let posted = h.asked().await;
        assert!(
            posted
                .text
                .contains("sudo needs the sudo password to run:\n\n    apt install ripgrep\n"),
            "{}",
            posted.text
        );
        assert!(posted.text.contains("/password <pw>"), "{}", posted.text);
        assert!(
            posted.text.contains("deleted afterwards"),
            "{}",
            posted.text
        );
        let refused = answer(&h.slot, Answer::Grant(Grant::Always));
        assert!(
            refused.unwrap_err().contains("/password <pw>"),
            "the password ask took a grant"
        );
        let text = answer(&h.slot, Answer::Password("hunter22".into())).unwrap();
        assert_eq!(text, "Password sent to sudo.");
        assert_eq!(receive.await.unwrap().as_deref(), Some("hunter22"));

        // /deny on a password ask cancels the sudo call.
        let (reply, receive) = oneshot::channel();
        h.prompts
            .send(Ask::Password(password_prompt(reply)))
            .await
            .unwrap();
        h.asked().await;
        let text = answer(&h.slot, Answer::No).unwrap();
        assert!(text.contains("does not run"), "{text}");
        assert_eq!(receive.await.unwrap(), None);

        // A re-ask says sudo refused the last one, and a child's ask is
        // named as the child's.
        let (reply, _receive) = oneshot::channel();
        h.prompts
            .send(Ask::Password(PasswordPrompt {
                refused: true,
                session_id: "another-session".into(),
                agent: Some("reviewer".into()),
                ..password_prompt(reply)
            }))
            .await
            .unwrap();
        let posted = h.asked().await;
        assert!(
            posted.text.contains("refused the last password"),
            "{}",
            posted.text
        );
        assert!(
            posted.text.contains("sudo (reviewer subagent) needs"),
            "{}",
            posted.text
        );
        h.cancel.cancel();
    }

    /// A password ask nobody answers times out like a grant ask.
    #[tokio::test]
    async fn a_password_nobody_types_is_a_no() {
        let mut h = harness(std::time::Duration::from_millis(50));
        let (reply, receive) = oneshot::channel();
        h.prompts
            .send(Ask::Password(password_prompt(reply)))
            .await
            .unwrap();
        let _ask = h.outbound.recv().await.unwrap();
        assert_eq!(receive.await.unwrap(), None);
        let verdict = h.outbound.recv().await.unwrap();
        assert!(verdict.text.contains("(no answer)"), "{}", verdict.text);
        assert!(h.slot.lock().unwrap().is_none());
        h.cancel.cancel();
    }

    #[tokio::test]
    async fn no_answer_in_time_is_a_no_and_the_chat_hears_it() {
        let mut h = harness(std::time::Duration::from_millis(50));
        let (reply, receive) = oneshot::channel();
        h.prompts.send(Ask::Grant(prompt(reply))).await.unwrap();
        let _ask = h.outbound.recv().await.unwrap();
        assert_eq!(receive.await.unwrap(), None);
        let verdict = h.outbound.recv().await.unwrap();
        assert_eq!(verdict.text, "GITHUB_TOKEN denied for bash (no answer).");
        assert!(h.slot.lock().unwrap().is_none());
        h.cancel.cancel();
    }

    #[test]
    fn a_childs_ask_is_marked_and_the_verdicts_say_what_takes_it_back() {
        let (reply, _receive) = oneshot::channel();
        let prompt = prompt(reply);
        let own = Asker::of(&prompt, "s");
        assert_eq!(own.shown, "bash");
        let child = Asker::of(&prompt, "another-session");
        assert_eq!(child.shown, "bash (subagent)");
        assert_eq!(child.tool, "bash");
        assert!(
            ask_text(&prompt, &child).contains("bash (subagent) wants GITHUB_TOKEN"),
            "{}",
            ask_text(&prompt, &child)
        );
        // The revoke line names the tool, not the mark, and only that
        // tool: `ilar secret revoke NAME` alone revokes every one.
        assert_eq!(
            decided("GITHUB_TOKEN", &child, Some(Grant::Always)),
            "GITHUB_TOKEN allowed for bash (subagent) from now on; ilar secret revoke \
             GITHUB_TOKEN --tool bash takes it back."
        );
        assert_eq!(
            decided("GITHUB_TOKEN", &own, None),
            "GITHUB_TOKEN denied for bash."
        );
        assert_eq!(
            no_answer("GITHUB_TOKEN", &own),
            "GITHUB_TOKEN denied for bash (no answer)."
        );
    }

    /// A bubble as Delta Chat counts it: a display line per 100
    /// characters or line break, folded past 34.
    fn bubble_lines(text: &str) -> usize {
        text.lines()
            .map(|line| line.chars().count().max(1).div_ceil(ASK_LINE_CHARS))
            .sum()
    }

    #[test]
    fn a_long_command_is_clipped_with_a_tail_so_the_ask_stays_one_message() {
        // Short and multi-line: shown whole, every line indented.
        assert_eq!(shown_command("one\ntwo"), "    one\n    two\n");
        assert_eq!(shown_command("  "), "    (no command)\n");
        let many = (1..=25)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let shown = shown_command(&many);
        assert!(shown.contains("    line 20\n"), "{shown}");
        assert!(!shown.contains("line 21"), "{shown}");
        assert!(shown.ends_with("    … 5 more lines not shown\n"), "{shown}");
        // One line longer than any message: cut where it is cut.
        let long = "x".repeat(2000);
        let shown = shown_command(&long);
        assert!(shown.starts_with("    xxx"), "{shown}");
        assert!(shown.ends_with("…\n"), "{shown}");
        assert_eq!(bubble_lines(&shown), ASK_COMMAND_LINES);
        // Short lines cost a display line each: what is left over is
        // what the long last line may take, not a fresh budget.
        let mixed = format!("{}\n{long}", "x\n".repeat(18));
        assert_eq!(bubble_lines(&shown_command(&mixed)), ASK_COMMAND_LINES);
        // Either ask fits one bubble, header, tail and all.
        for detail in [many, long, mixed] {
            let (reply, _receive) = oneshot::channel();
            let mut asking = prompt(reply);
            asking.detail = detail.clone();
            let asker = Asker::of(&asking, "s");
            assert!(bubble_lines(&ask_text(&asking, &asker)) <= 34);
            let (reply, _receive) = oneshot::channel();
            let mut asking = password_prompt(reply);
            asking.detail = detail;
            asking.refused = true;
            let asker = Asker::of_password(&asking, "s");
            let lines = bubble_lines(&password_ask_text(&asking, &asker));
            assert!(lines <= 34, "{lines}");
        }
    }

    #[tokio::test]
    async fn a_turn_that_ends_takes_its_ask_with_it() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts.send(Ask::Grant(prompt(reply))).await.unwrap();
        let _ask = h.outbound.recv().await.unwrap();
        drop(receive);
        let over = h.outbound.recv().await.unwrap();
        assert!(over.text.contains("stopped waiting"), "{}", over.text);
        assert_eq!(
            answer(&h.slot, Answer::Grant(Grant::Once)),
            Err("Nothing is waiting for a grant.")
        );
        // A password ask goes the same way.
        let (reply, receive) = oneshot::channel();
        h.prompts
            .send(Ask::Password(password_prompt(reply)))
            .await
            .unwrap();
        let _ask = h.outbound.recv().await.unwrap();
        drop(receive);
        let over = h.outbound.recv().await.unwrap();
        assert!(over.text.contains("stopped waiting"), "{}", over.text);
        assert_eq!(
            answer(&h.slot, Answer::Password("hunter22".into())),
            Err("Nothing is waiting for a grant.")
        );
        h.cancel.cancel();
    }
}
