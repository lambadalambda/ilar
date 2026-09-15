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
    /// A command typed without what it needs: its usage line, and not
    /// the whole help on top of it.
    Usage(&'static str),
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
    /// A known command whose argument does not read as one: the text is
    /// the whole reply, since "No command /grant" would be a lie.
    Misread(String),
}

/// What a person on a chat does about a sealed secret store, for every
/// refusal the lock causes.
pub const UNLOCK_HINT: &str = "send /unlock <master password> in this chat";

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
            Ok(approval) => Command::Grant(approval),
            Err(message) => Command::Misread(message),
        },
        ("deny", _) => Command::Deny,
        ("unlock", Some(password)) => Command::Unlock(password.to_string()),
        ("unlock", None) => Command::Usage(UNLOCK_USAGE),
        ("compact", _) => Command::Compact,
        ("help", _) => Command::Help,
        ("pending", _) => Command::Pending,
        ("approve", argument) => Command::Approve(argument.unwrap_or("all").to_string()),
        ("reject", argument) => Command::Reject(argument.unwrap_or("all").to_string()),
        (other, _) => Command::Unknown(other.to_string()),
    })
}

/// `[once|session|always] [password]`: the span first, the password —
/// sudo's, when the ask wanted one — as everything after it. A first
/// word that reads like a misspelt span is refused rather than taken
/// as the start of a password: `/grant sesion hunter2` used to grant
/// once with the password "sesion hunter2".
fn parse_grant(argument: &str) -> Result<ilar::secrets::Approval, String> {
    use ilar::secrets::{Approval, Grant};
    let argument = argument.trim();
    let (span, rest) = match argument.split_once(char::is_whitespace) {
        Some((span, rest)) => (span, rest.trim()),
        None => (argument, ""),
    };
    // Case-blind: a phone capitalises the first word.
    let (grant, password) = match span.to_ascii_lowercase().as_str() {
        "" | "once" => (Grant::Once, rest),
        "session" => (Grant::Session, rest),
        "always" => (Grant::Always, rest),
        // No span word: the whole argument is the password — unless it
        // was meant to be a span.
        _ => match misspelt_span(span) {
            Some(meant) => {
                return Err(format!(
                    "{span}? The spans are once, session and always — /grant {meant} … if that \
                     is what you meant. A password goes after the span.",
                ));
            }
            None => (Grant::Once, argument),
        },
    };
    Ok(Approval {
        grant,
        password: (!password.is_empty()).then(|| password.to_string()),
    })
}

/// The span a word was probably trying to be: within two edits of one,
/// and long enough for that to mean something. A password is left
/// alone — nothing that far from "once" was a typo of it.
fn misspelt_span(word: &str) -> Option<&'static str> {
    const SPANS: [&str; 3] = ["once", "session", "always"];
    let word = word.to_ascii_lowercase();
    (word.chars().count() >= 4)
        .then(|| SPANS.into_iter().find(|span| edits_within(&word, span, 2)))
        .flatten()
}

/// Whether `a` becomes `b` in at most `limit` insertions, deletions or
/// substitutions (Levenshtein).
fn edits_within(a: &str, b: &str, limit: usize) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > limit {
        return false;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(left != right);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()] <= limit
}

/// `/unlock` with nothing after it: the password is the whole point.
pub const UNLOCK_USAGE: &str =
    "/unlock <master password> — the password goes after the command, in the same message.";

pub const HELP: &str = "/new — start a fresh chat (memory stays); a turn running here is cancelled\n\
/model — list the models; /model <provider/model> switches; add --save to make it the default for new chats\n\
/abort (or /stop) — cancel the turn running now; messages that were waiting run after it\n\
/grant [session|always] [password], /deny — answer a tool's ask for a stored secret or for root\n\
/unlock <master password> — open a sealed secret store for this gateway process; the password is taken back out of the chat where the channel allows it\n\
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
            parse("/grant Always"),
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
        // A usage line, not the whole help on top of a refusal.
        assert_eq!(parse("/unlock"), Some(Command::Usage(UNLOCK_USAGE)));
        assert!(!UNLOCK_USAGE.starts_with("No command"));
    }

    /// A misspelt span is said out loud: taken as a password it would
    /// answer the ask with the typo in it and grant once.
    #[test]
    fn a_misspelt_span_is_not_a_password() {
        let Some(Command::Misread(message)) = parse("/grant sesion hunter2") else {
            panic!("a typo read as a password");
        };
        assert!(message.contains("sesion?"), "{message}");
        assert!(message.contains("/grant session"), "{message}");
        assert!(matches!(parse("/grant alwyas"), Some(Command::Misread(_))));
        assert!(matches!(parse("/grant onse"), Some(Command::Misread(_))));
        // A password that is nothing like a span stays a password, and
        // so does one too short to be a typo of anything.
        assert!(matches!(parse("/grant hunter2"), Some(Command::Grant(_))));
        assert!(matches!(
            parse("/grant sessionkeyaddendum"),
            Some(Command::Grant(_))
        ));
        assert!(matches!(parse("/grant abc"), Some(Command::Grant(_))));
        assert!(edits_within("sesion", "session", 2));
        assert!(!edits_within("hunter2", "session", 2));
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

    /// Every alias the parser takes is a command a person can find.
    #[test]
    fn the_help_names_the_aliases_too() {
        assert!(HELP.contains("/abort (or /stop)"), "{HELP}");
    }
}
