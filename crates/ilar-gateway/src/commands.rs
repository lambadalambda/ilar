//! Slash commands a person types into the chat. Handled by the gateway
//! itself, before any model is involved.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start over: a fresh session for this chat. Memory stays.
    New,
    /// List the models, or switch to one; `save` makes it the default
    /// for new chats as well.
    Model {
        model: Option<String>,
        save: bool,
    },
    /// Cancel the turn running on this chat.
    Abort,
    /// Answer a tool's ask for a secret: once, this session, or always,
    /// with the sudo password when the ask wanted one.
    Grant(ilar::secrets::Approval),
    /// Refuse it.
    Deny,
    /// The secret store's master password, for this gateway process.
    Unlock(String),
    /// Replace this chat's conversation with one handover summary.
    Compact,
    Help,
    /// What the review staged and has not been approved.
    Pending,
    /// Apply a staged plan by id, or `all`.
    Approve(String),
    /// Drop a staged plan by id, or `all`.
    Reject(String),
    Unknown(String),
}

/// `Some` when the text is a command: a slash, a word, maybe an
/// argument. Anything else is a message for the model.
pub fn parse(text: &str) -> Option<Command> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    let argument = parts.next().map(str::trim).filter(|s| !s.is_empty());
    if name.is_empty() {
        return None;
    }
    Some(match (name, argument) {
        ("new", _) => Command::New,
        ("model", argument) => {
            let words: Vec<&str> = argument.unwrap_or_default().split_whitespace().collect();
            Command::Model {
                model: words
                    .iter()
                    .find(|w| **w != "--save")
                    .map(|w| w.to_string()),
                save: words.contains(&"--save"),
            }
        }
        ("abort" | "stop", _) => Command::Abort,
        ("grant", argument) => match parse_grant(argument.unwrap_or_default()) {
            Some(approval) => Command::Grant(approval),
            None => Command::Unknown(format!("grant {}", argument.unwrap_or_default())),
        },
        ("deny", _) => Command::Deny,
        ("unlock", Some(password)) => Command::Unlock(password.to_string()),
        ("unlock", None) => Command::Unknown("unlock (the master password goes after it)".into()),
        ("compact", _) => Command::Compact,
        ("help", _) => Command::Help,
        ("pending", _) => Command::Pending,
        ("approve", argument) => Command::Approve(argument.unwrap_or("all").to_string()),
        ("reject", argument) => Command::Reject(argument.unwrap_or("all").to_string()),
        (other, _) => Command::Unknown(other.to_string()),
    })
}

/// `[once|session|always] [password]`: the span first, the password —
/// sudo's, when the ask wanted one — as everything after it.
fn parse_grant(argument: &str) -> Option<ilar::secrets::Approval> {
    use ilar::secrets::{Approval, Grant};
    let argument = argument.trim();
    let (span, rest) = match argument.split_once(char::is_whitespace) {
        Some((span, rest)) => (span, rest.trim()),
        None => (argument, ""),
    };
    let (grant, password) = match span {
        "" | "once" => (Grant::Once, rest),
        "session" => (Grant::Session, rest),
        "always" => (Grant::Always, rest),
        // No span word: the whole argument is the password.
        _ => (Grant::Once, argument),
    };
    Some(Approval {
        grant,
        password: (!password.is_empty()).then(|| password.to_string()),
    })
}

pub const HELP: &str = "/new — start a fresh chat (memory stays)\n\
/model — list the models; /model <provider/model> switches; add --save to make it the default for new chats\n\
/abort — cancel the turn running now; messages that were waiting run after it\n\
/grant [session|always] [password], /deny — answer a tool's ask for a stored secret or for root\n\
/unlock <master password> — open a sealed secret store for this gateway process\n\
/compact — replace the conversation with one handover summary; memory stays\n\
/pending — what the review wants to remember, when approval is on\n\
/approve [id|all], /reject [id|all] — decide on it\n\
/help — this";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_and_prose_does_not() {
        assert_eq!(parse("/new"), Some(Command::New));
        assert_eq!(parse("  /new please"), Some(Command::New));
        assert_eq!(
            parse("/model"),
            Some(Command::Model {
                model: None,
                save: false
            })
        );
        assert_eq!(
            parse("/model --save"),
            Some(Command::Model {
                model: None,
                save: true
            })
        );
        assert_eq!(
            parse("/model zai/glm-4.7 --save"),
            Some(Command::Model {
                model: Some("zai/glm-4.7".into()),
                save: true
            })
        );
        assert_eq!(
            parse("/model zai/glm-4.7"),
            Some(Command::Model {
                model: Some("zai/glm-4.7".into()),
                save: false
            })
        );
        assert_eq!(
            parse("/model   "),
            Some(Command::Model {
                model: None,
                save: false
            })
        );
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/abort"), Some(Command::Abort));
        assert_eq!(parse("/compact"), Some(Command::Compact));
        assert_eq!(parse("/stop now"), Some(Command::Abort));
        use ilar::secrets::{Approval, Grant};
        assert_eq!(
            parse("/grant"),
            Some(Command::Grant(Approval::from(Grant::Once)))
        );
        assert_eq!(
            parse("/grant always"),
            Some(Command::Grant(Approval::from(Grant::Always)))
        );
        assert_eq!(
            parse("/grant session "),
            Some(Command::Grant(Approval::from(Grant::Session)))
        );
        assert_eq!(
            parse("/grant session hunter two"),
            Some(Command::Grant(Approval {
                grant: Grant::Session,
                password: Some("hunter two".into())
            }))
        );
        assert_eq!(
            parse("/grant hunter2"),
            Some(Command::Grant(Approval {
                grant: Grant::Once,
                password: Some("hunter2".into())
            }))
        );
        assert_eq!(parse("/deny"), Some(Command::Deny));
        assert_eq!(
            parse("/unlock open sesame"),
            Some(Command::Unlock("open sesame".into()))
        );
        assert!(matches!(parse("/unlock"), Some(Command::Unknown(_))));
        assert_eq!(parse("/pending"), Some(Command::Pending));
        assert_eq!(parse("/approve"), Some(Command::Approve("all".into())));
        assert_eq!(
            parse("/approve ab12"),
            Some(Command::Approve("ab12".into()))
        );
        assert_eq!(parse("/reject all"), Some(Command::Reject("all".into())));
        assert_eq!(parse("/dance"), Some(Command::Unknown("dance".into())));
        assert_eq!(parse("/"), None);
        assert_eq!(parse("what about /new?"), None);
        assert_eq!(parse("1/2 done"), None);
    }
}
