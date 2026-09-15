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
    pub tool: String,
    /// The ask takes a password (sudo's); one sent to any other ask is
    /// refused rather than held as the sudo password by mistake.
    pub password_wanted: bool,
    answer: oneshot::Sender<Option<Approval>>,
}

/// One seat's slot for the ask in flight: a tool blocks on its answer,
/// so there is never more than one.
pub type PendingSlot = Arc<Mutex<Option<PendingGrant>>>;

/// What was decided, for the chat.
pub fn decided(secret: &str, tool: &str, grant: Option<Grant>) -> String {
    match grant {
        Some(Grant::Once) => format!("{secret} allowed for {tool}, this once."),
        Some(Grant::Session) => {
            format!("{secret} allowed for {tool} until this chat is restarted.")
        }
        Some(Grant::Always) => {
            format!(
                "{secret} allowed for {tool} from now on; ilar secret revoke {secret} takes it back."
            )
        }
        None => format!("{secret} denied for {tool}."),
    }
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
        &pending.tool,
        approval.as_ref().map(|approval| approval.grant),
    );
    pending
        .answer
        .send(approval)
        .map(|()| text)
        .map_err(|_| "The tool stopped waiting for that.")
}

/// The message the chat gets.
pub fn ask_text(prompt: &GrantPrompt) -> String {
    let purpose = if prompt.description.is_empty() {
        String::new()
    } else {
        format!(" ({})", prompt.description)
    };
    let password = if prompt.password_wanted {
        " Put the sudo password last (/grant session <password>) if the system wants one; \
         it is held in memory until the gateway stops."
    } else {
        ""
    };
    format!(
        "🔑 {} wants {}{purpose} to run:\n{}\n/grant allows it this once, /grant session or \
         /grant always for longer, /deny refuses. Unanswered in {} minutes, it is a no.{password}",
        prompt.tool,
        prompt.secret,
        prompt.detail,
        GRANT_TIMEOUT.as_secs() / 60
    )
}

/// Post every ask to the chat and relay the chat's answer. Ends with
/// the runtime (the receiver closes) or the gateway (`cancel`).
pub async fn watch(
    mut prompts: GrantReceiver,
    slot: PendingSlot,
    outbound: mpsc::Sender<Outbound>,
    channel: String,
    chat_id: String,
    timeout: std::time::Duration,
    cancel: CancellationToken,
) {
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
        post(ask_text(&prompt)).await;
        let (answer_tx, answer_rx) = oneshot::channel();
        *slot.lock().unwrap() = Some(PendingGrant {
            secret: prompt.secret.clone(),
            tool: prompt.tool.clone(),
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
                post(decided(&prompt.secret, &prompt.tool, None) + " (no answer)").await;
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
            "deltachat".into(),
            "12".into(),
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
        assert!(
            posted
                .text
                .contains("bash wants GITHUB_TOKEN (for gh) to run:\ngh pr list"),
            "{}",
            posted.text
        );
        assert!(posted.text.contains("/grant"), "{}", posted.text);
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
        assert_eq!(verdict.text, "GITHUB_TOKEN denied for bash. (no answer)");
        assert!(h.slot.lock().unwrap().is_none());
        h.cancel.cancel();
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
