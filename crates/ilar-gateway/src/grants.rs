//! A tool's ask for a secret, answered from the chat. Nobody sits at a
//! channel to fill in a form, but a yes or no fits in a message: the
//! ask is posted to the chat the seat belongs to, and `/grant` or
//! `/deny` answers it. Unanswered long enough, it is a no.

use std::sync::{Arc, Mutex};

use ilar::secrets::{Approval, Grant, GrantPrompt, GrantReceiver};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::bus::Outbound;

/// How long an ask waits for the chat before it is refused.
pub const GRANT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The ask the chat has not answered yet.
pub struct PendingGrant {
    pub secret: String,
    pub asker: Asker,
    /// The ask takes a password (sudo's); one sent to any other ask is
    /// refused rather than held as the sudo password by mistake.
    pub password_wanted: bool,
    answer: oneshot::Sender<Option<Approval>>,
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
        let shown = if prompt.session_id == session_id {
            prompt.tool.clone()
        } else {
            format!("{} (subagent)", prompt.tool)
        };
        Self {
            shown,
            tool: prompt.tool.clone(),
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
/// was answered.
pub fn answer(slot: &PendingSlot, approval: Option<Approval>) -> Result<String, &'static str> {
    let pending = slot.lock().unwrap().take();
    let Some(pending) = pending else {
        return Err("Nothing is waiting for a grant.");
    };
    if !pending.password_wanted
        && approval
            .as_ref()
            .is_some_and(|approval| approval.password.is_some())
    {
        // Back into the slot: the ask stands, the answer was not one.
        *slot.lock().unwrap() = Some(pending);
        return Err("That ask takes no password: /grant, /grant session or /grant always alone.");
    }
    let text = decided(
        &pending.secret,
        &pending.asker,
        approval.as_ref().map(|approval| approval.grant),
    );
    pending
        .answer
        .send(approval)
        .map(|()| text)
        .map_err(|_| "The tool stopped waiting for that.")
}

/// The most of a command one ask shows: Delta Chat folds a bubble past
/// 34 display lines of 100 characters, and the ask's own words have to
/// fit beside the command. Past this the command gets a tail naming
/// what is not shown, instead of the header, the command and the
/// instructions arriving as separate bubbles.
const ASK_COMMAND_LINES: usize = 20;
const ASK_COMMAND_CHARS: usize = 1200;

/// The command as the ask shows it: verbatim, every line indented, so
/// its last line cannot be read as part of the instructions, and
/// clipped with a tail when it is longer than one message can hold.
fn shown_command(detail: &str) -> String {
    let lines: Vec<&str> = detail.lines().collect();
    let mut shown = String::new();
    let mut chars = 0;
    let mut whole = 0;
    for line in lines.iter().take(ASK_COMMAND_LINES) {
        let room = ASK_COMMAND_CHARS - chars;
        if line.chars().count() > room {
            // Cut, and marked as cut where the cut is.
            let cut: String = line.chars().take(room).collect();
            shown.push_str(&format!("    {cut}…\n"));
            whole += 1;
            break;
        }
        shown.push_str(&format!("    {line}\n"));
        chars += line.chars().count();
        whole += 1;
    }
    let hidden = lines.len() - whole;
    if hidden > 0 {
        shown.push_str(&format!(
            "    … {hidden} more line{} not shown\n",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    shown
}

/// The message the chat gets. `session_id` is the seat's own session:
/// an ask from another one is a subagent's, and says so.
pub fn ask_text(prompt: &GrantPrompt, session_id: &str) -> String {
    let purpose = if prompt.description.is_empty() {
        String::new()
    } else {
        format!(" ({})", prompt.description)
    };
    let password = if prompt.password_wanted {
        " Put the sudo password last (/grant session <password>) if the system wants one; \
         it is held in memory until this chat is restarted."
    } else {
        ""
    };
    format!(
        "🔑 {} wants {}{purpose} to run:\n\n{}\n/grant allows it this once, /grant session or \
         /grant always for longer, /deny refuses. Unanswered in {} minutes, it is a no.{password}",
        Asker::of(prompt, session_id).shown,
        prompt.secret,
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

/// Post every ask to the chat and relay the chat's answer. Ends with
/// the runtime (the receiver closes) or the gateway (`cancel`).
pub async fn watch(
    mut prompts: GrantReceiver,
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
        let prompt = tokio::select! {
            prompt = prompts.recv() => match prompt {
                Some(prompt) => prompt,
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
        post(ask_text(&prompt, &session_id)).await;
        let asker = Asker::of(&prompt, &session_id);
        let (answer_tx, answer_rx) = oneshot::channel();
        *slot.lock().unwrap() = Some(PendingGrant {
            secret: prompt.secret.clone(),
            asker: asker.clone(),
            password_wanted: prompt.password_wanted,
            answer: answer_tx,
        });
        let mut reply = prompt.reply;
        tokio::select! {
            answer = answer_rx => {
                // The command took the slot and decided; the tool gets
                // what the chat said, and a chat that already moved on
                // (the sender dropped unanswered) gets a no.
                let _ = reply.send(answer.unwrap_or(None));
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
                let _ = reply.send(None);
                post(no_answer(&prompt.secret, &asker)).await;
            }
            _ = cancel.cancelled() => {
                slot.lock().unwrap().take();
                let _ = reply.send(None);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(reply: oneshot::Sender<Option<Approval>>) -> GrantPrompt {
        GrantPrompt {
            session_id: "s".into(),
            tool_call_id: None,
            tool: "bash".into(),
            secret: "GITHUB_TOKEN".into(),
            description: "for gh".into(),
            detail: "gh pr list".into(),
            password_wanted: false,
            reply,
        }
    }

    struct Harness {
        prompts: mpsc::Sender<GrantPrompt>,
        slot: PendingSlot,
        outbound: mpsc::Receiver<Outbound>,
        cancel: CancellationToken,
    }

    fn harness(timeout: std::time::Duration) -> Harness {
        let (prompts, receiver) = ilar::secrets::grant_channel(1);
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

    #[tokio::test]
    async fn the_ask_is_posted_and_the_chats_answer_reaches_the_tool() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts.send(prompt(reply)).await.unwrap();
        let posted = h.outbound.recv().await.unwrap();
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
        // The slot is filled once the ask is posted.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while h.slot.lock().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // A password nobody asked for is refused and the ask stands.
        let refused = answer(
            &h.slot,
            Some(Approval {
                grant: Grant::Once,
                password: Some("typo".into()),
            }),
        );
        assert!(refused.unwrap_err().contains("takes no password"));
        let text = answer(&h.slot, Some(Approval::from(Grant::Session))).unwrap();
        assert!(
            text.starts_with("GITHUB_TOKEN allowed for bash until"),
            "{text}"
        );
        assert_eq!(receive.await.unwrap(), Some(Approval::from(Grant::Session)));
        assert_eq!(
            answer(&h.slot, None),
            Err("Nothing is waiting for a grant.")
        );
        h.cancel.cancel();
    }

    #[tokio::test]
    async fn no_answer_in_time_is_a_no_and_the_chat_hears_it() {
        let mut h = harness(std::time::Duration::from_millis(50));
        let (reply, receive) = oneshot::channel();
        h.prompts.send(prompt(reply)).await.unwrap();
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
            ask_text(&prompt, "another-session").contains("bash (subagent) wants GITHUB_TOKEN"),
            "{}",
            ask_text(&prompt, "another-session")
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

    #[test]
    fn a_long_command_is_clipped_with_a_tail_so_the_ask_stays_one_message() {
        // Short and multi-line: shown whole, every line indented.
        assert_eq!(shown_command("one\ntwo"), "    one\n    two\n");
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
        assert_eq!(shown, format!("    {}…\n", "x".repeat(ASK_COMMAND_CHARS)));
        // The whole ask fits one Delta Chat bubble, header and all.
        let (reply, _receive) = oneshot::channel();
        let mut prompt = prompt(reply);
        prompt.detail = many;
        let text = ask_text(&prompt, "s");
        let display_lines: usize = text
            .lines()
            .map(|line| line.chars().count().max(1).div_ceil(100))
            .sum();
        assert!(display_lines <= 34, "{display_lines}");
    }

    #[tokio::test]
    async fn a_turn_that_ends_takes_its_ask_with_it() {
        let mut h = harness(GRANT_TIMEOUT);
        let (reply, receive) = oneshot::channel();
        h.prompts.send(prompt(reply)).await.unwrap();
        let _ask = h.outbound.recv().await.unwrap();
        drop(receive);
        let over = h.outbound.recv().await.unwrap();
        assert!(over.text.contains("stopped waiting"), "{}", over.text);
        assert_eq!(
            answer(&h.slot, Some(Approval::from(Grant::Once))),
            Err("Nothing is waiting for a grant.")
        );
        h.cancel.cancel();
    }
}
