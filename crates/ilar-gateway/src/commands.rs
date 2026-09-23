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
    /// Answer a tool's ask for a secret: once, this session, or always.
    /// `ask` is set by a tapped button, which names the ask it answers.
    Grant {
        grant: ilar::secrets::Grant,
        ask: Option<String>,
    },
    /// The sudo password, for the ask that comes after the yes.
    Password(String),
    /// Refuse it — either ask; `ask` as for [`Command::Grant`].
    Deny {
        ask: Option<String>,
    },
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
    /// Apply a staged plan by id, or `all`; with neither, list and ask.
    Approve(Option<String>),
    /// Drop a staged plan by id, or `all`; with neither, list and ask.
    Reject(Option<String>),
    Unknown(String),
    /// A misspelt `/unlock` or `/password` that carried an argument:
    /// nothing was unlocked and the argument was probably the password.
    MistypedSecret {
        typed: String,
        meant: &'static str,
    },
    /// A known command whose argument does not read as one: the text is
    /// the whole reply, since "No command /grant" would be a lie.
    Misread(String),
    /// The chat's model, whether a turn runs, subagents, asks, context.
    Status,
    /// What this session has cost, over its whole log.
    Cost,
    /// The scheduled jobs; `remove` takes one away by id or unique name.
    Cron {
        remove: Option<String>,
    },
    /// Subagents running for this chat, and results held for delivery.
    Tasks,
    /// The sender and chat ids as the channel reports them.
    Whoami,
    /// Drain and exit with the code the service unit restarts on.
    Restart,
}

impl Command {
    /// Whether the message this was parsed from has a password in it,
    /// and so is taken back out of the chat before anything else. Wrong
    /// password, wrong command, wrong spelling: it is in the history
    /// all the same.
    pub fn carries_a_secret(&self) -> bool {
        match self {
            Command::Password(_) | Command::Unlock(_) | Command::MistypedSecret { .. } => true,
            Command::Misread(text) => text == PASSWORD_AFTER_THE_YES,
            _ => false,
        }
    }
}

impl Command {
    /// The name and the reason, for a command that reaches past the chat
    /// it is sent in — the person's memory, every chat's model, the
    /// process, a password — and so is refused in a room, where any
    /// allowlisted member could send it.
    pub fn private_only(&self) -> Option<(&'static str, &'static str)> {
        let memory = "decides what I remember about you";
        let password = "carries a password";
        Some(match self {
            Command::Pending => ("pending", "shows what I would remember about you"),
            Command::Approve(_) => ("approve", memory),
            Command::Reject(_) => ("reject", memory),
            Command::Restart => ("restart", "restarts me for every chat"),
            Command::Model { save: true, .. } => {
                ("model … --save", "changes the default model for every chat")
            }
            Command::Unlock(_) => ("unlock", password),
            Command::Password(_) => ("password", password),
            _ => return None,
        })
    }
}

/// The menu a room is offered: [`MENU`] without the commands a room is
/// refused (see [`Command::private_only`]).
pub fn group_menu() -> impl Iterator<Item = (&'static str, &'static str)> {
    const PRIVATE: &[&str] = &[
        "password", "unlock", "pending", "approve", "reject", "restart",
    ];
    MENU.iter()
        .copied()
        .filter(|(name, _)| !PRIVATE.contains(name))
}

/// What a person on a chat does about a sealed secret store, for every
/// refusal the lock causes. The store is the process's, so a private
/// chat opens it for a room too — and a room is no place to type it.
pub const UNLOCK_HINT: &str = "send me /unlock <master password> in a private chat";

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
    // Case-blind: a phone capitalises the first word of a message, and
    // "/Help" is the same ask as "/help".
    let lowercase = name.to_ascii_lowercase();
    Some(match (lowercase.as_str(), argument) {
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
        ("grant", argument) => {
            let (argument, ask) = split_ask(argument.unwrap_or_default());
            match parse_grant(argument) {
                Ok(grant) => Command::Grant { grant, ask },
                Err(message) => Command::Misread(message),
            }
        }
        ("deny", argument) => Command::Deny {
            ask: split_ask(argument.unwrap_or_default()).1,
        },
        ("password", Some(password)) => Command::Password(password.to_string()),
        ("password", None) => Command::Usage(PASSWORD_USAGE),
        ("unlock", Some(password)) => Command::Unlock(password.to_string()),
        ("unlock", None) => Command::Usage(UNLOCK_USAGE),
        ("compact", _) => Command::Compact,
        ("help", _) => Command::Help,
        ("pending", _) => Command::Pending,
        // No argument is not "all": Telegram sends a menu entry, and the
        // `/approve` in a staging message, as the bare word on one tap.
        ("approve", argument) => Command::Approve(argument.map(str::to_string)),
        ("reject", argument) => Command::Reject(argument.map(str::to_string)),
        ("status", _) => Command::Status,
        ("cost" | "usage", _) => Command::Cost,
        ("cron", None) => Command::Cron { remove: None },
        ("cron", Some(argument)) => match argument.split_once(char::is_whitespace) {
            Some((verb, which))
                if verb.eq_ignore_ascii_case("remove") && !which.trim().is_empty() =>
            {
                Command::Cron {
                    remove: Some(which.trim().to_string()),
                }
            }
            _ => Command::Misread(CRON_USAGE.to_string()),
        },
        ("tasks", _) => Command::Tasks,
        ("whoami", _) => Command::Whoami,
        ("restart", _) => Command::Restart,
        // A near-miss of a command that takes a password, with
        // something after it: that something is the password, and no
        // command ran to take it back out. Echoed as it was typed
        // otherwise — the refusal is about a word the person wrote,
        // not about our lowercasing of it.
        (_, argument) => match argument.and_then(|_| mistyped_secret(&lowercase)) {
            Some(meant) => Command::MistypedSecret {
                typed: name.to_string(),
                meant,
            },
            None => Command::Unknown(name.to_string()),
        },
    })
}

/// A trailing `#id` — what a grant ask's buttons append to name the ask
/// they answer — split off the rest of the argument. Only the buttons'
/// exact form: `#` and six lowercase hex digits. Anything else stays in
/// the argument, so `/grant #hunter2` is still read as a password typed
/// in the wrong place, and taken back out of the chat.
fn split_ask(argument: &str) -> (&str, Option<String>) {
    let argument = argument.trim();
    let (rest, last) = argument
        .rsplit_once(char::is_whitespace)
        .unwrap_or(("", argument));
    match last.strip_prefix('#') {
        Some(id) if id.len() == 6 && id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) => {
            (rest.trim(), Some(id.to_string()))
        }
        _ => (argument, None),
    }
}

fn parse_grant(argument: &str) -> Result<ilar::secrets::Grant, String> {
    use ilar::secrets::Grant;
    let argument = argument.trim();
    let (span, rest) = match argument.split_once(char::is_whitespace) {
        Some((span, rest)) => (span, rest.trim()),
        None => (argument, ""),
    };
    // Case-blind: a phone capitalises the first word.
    let grant = match span.to_ascii_lowercase().as_str() {
        "" | "once" => Grant::Once,
        "session" => Grant::Session,
        "always" => Grant::Always,
        _ => {
            return Err(match misspelt_span(span) {
                Some(meant) => format!(
                    "{span}? The spans are once, session and always — /grant {meant} if that is \
                     what you meant.",
                ),
                None => PASSWORD_AFTER_THE_YES.to_string(),
            });
        }
    };
    if !rest.is_empty() {
        return Err(PASSWORD_AFTER_THE_YES.to_string());
    }
    Ok(grant)
/// `[once|session|always]`, and nothing else: the approval question
/// takes a span and no password. A word that reads like a misspelt span
/// says so — `/grant sesion hunter2` used to grant once with the
/// password "sesion hunter2" — and anything else is pointed at the
/// prompt the password belongs in.
}

/// What a password given with `/grant` is told. The ask it would answer
/// is the approval question; sudo's password is asked for on its own,
/// and only where sudo wants one. Public because the gateway takes that
/// message back out of the chat: whatever it was, it was meant to be a
/// password and it is in the history now.
pub const PASSWORD_AFTER_THE_YES: &str = "/grant takes a span and nothing else: once, session \
                                          or always. The password is asked for after the yes — \
                                          /password <pw> when sudo asks for it.";

/// The password-taking command a word was probably trying to be:
/// `/unlok hunter2` unlocks nothing and leaves the password in the
/// chat. Two edits, the same reach as a misspelt span. A false match
/// costs an unknown command's message, which did nothing anyway; a
/// miss costs a password sitting in the history for good.
fn mistyped_secret(word: &str) -> Option<&'static str> {
    const WITH_A_PASSWORD: [&str; 2] = ["unlock", "password"];
    WITH_A_PASSWORD
        .into_iter()
        .find(|command| edits_within(word, command, 2))
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

/// `/password` with nothing after it.
pub const PASSWORD_USAGE: &str = "/password <pw> — the sudo password goes after the command, in \
                                  the same message; it is deleted afterwards.";

/// `/cron` with an argument that is not `remove <id|name>`.
pub const CRON_USAGE: &str =
    "/cron lists the jobs; /cron remove <id|name> takes one away. Adding is done by asking.";

/// Every command a person can type, with a one-line description: what
/// a channel with a command menu (Telegram's `setMyCommands`) shows
/// when the person types `/`. One name per command — the aliases and
/// the arguments are in [`HELP`], which is the same list in prose.
pub const MENU: &[(&str, &str)] = &[
    ("new", "start a fresh chat; memory stays"),
    (
        "model",
        "list the models, or switch: /model <provider/model> [--save]",
    ),
    ("abort", "cancel the turn running now"),
    (
        "grant",
        "allow a tool's ask for a secret or root: [session|always]",
    ),
    ("deny", "refuse it"),
    ("password", "the sudo password, after a yes: /password <pw>"),
    (
        "unlock",
        "open a sealed secret store: /unlock <master password>",
    ),
    (
        "compact",
        "replace the conversation with one handover summary",
    ),
    (
        "pending",
        "what the review wants to remember, when approval is on",
    ),
    ("approve", "keep a staged memory: [id|all]"),
    ("reject", "drop a staged memory: [id|all]"),
    ("status", "the model, the turn, subagents, asks, context"),
    ("cost", "what this session has cost"),
    ("cron", "the scheduled jobs; /cron remove <id|name>"),
    ("tasks", "subagents running here, and results held"),
    (
        "whoami",
        "your ids as the channel reports them, for allow_from",
    ),
    ("restart", "drain and restart the gateway"),
    ("help", "the commands"),
];

pub const HELP: &str = "/new — start a fresh chat (memory stays); a turn running here is cancelled\n\
/model — list the models; /model <provider/model> switches; add --save to make it the default for new chats\n\
/abort (or /stop) — cancel the turn running now; messages that were waiting run after it\n\
/grant [session|always], /deny — answer a tool's ask for a stored secret or for root\n\
/password <pw> — the sudo password, when sudo asks for one after a yes; the message is deleted afterwards\n\
/unlock <master password> — open a sealed secret store for this gateway process; the password is taken back out of the chat where the channel allows it\n\
/compact — replace the conversation with one handover summary; memory stays\n\
/pending — what the review wants to remember, when approval is on\n\
/approve [id|all], /reject [id|all] — decide on it\n\
/status — this chat's model, whether a turn is running, subagents, asks waiting, context size\n\
/cost (or /usage) — what this session has cost, over its whole log\n\
/cron — the scheduled jobs; /cron remove <id|name> takes one away\n\
/tasks — subagents running for this chat, and results held for delivery\n\
/whoami — your sender and chat ids as the channel reports them, for allow_from\n\
/restart — drain and restart the gateway (the service unit starts it again)\n\
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
        use ilar::secrets::Grant;
        let typed = |grant| Some(Command::Grant { grant, ask: None });
        assert_eq!(parse("/grant"), typed(Grant::Once));
        assert_eq!(parse("/grant always"), typed(Grant::Always));
        assert_eq!(parse("/grant Always"), typed(Grant::Always));
        // A tapped button names its ask.
        assert_eq!(
            parse("/grant always #ab12cd"),
            Some(Command::Grant {
                grant: Grant::Always,
                ask: Some("ab12cd".into())
            })
        );
        assert_eq!(
            parse("/grant #ab12cd"),
            Some(Command::Grant {
                grant: Grant::Once,
                ask: Some("ab12cd".into())
            })
        );
        assert_eq!(
            parse("/deny #ab12cd"),
            Some(Command::Deny {
                ask: Some("ab12cd".into())
            })
        );
        assert_eq!(parse("/grant session "), typed(Grant::Session));
        assert_eq!(parse("/deny"), Some(Command::Deny { ask: None }));
        assert_eq!(
            parse("/password hunter two"),
            Some(Command::Password("hunter two".into()))
        );
        assert_eq!(
            parse("/Password hunter2"),
            Some(Command::Password("hunter2".into()))
        );
        assert_eq!(parse("/password"), Some(Command::Usage(PASSWORD_USAGE)));
        assert_eq!(
            parse("/unlock open sesame"),
            Some(Command::Unlock("open sesame".into()))
        );
        // A usage line, not the whole help on top of a refusal.
        assert_eq!(parse("/unlock"), Some(Command::Usage(UNLOCK_USAGE)));
        assert!(!UNLOCK_USAGE.starts_with("No command"));
    }

    /// `/grant` takes a span and nothing else: a password on it is
    /// pointed at the prompt that asks for one, and a misspelt span is
    /// still said out loud rather than read as a password.
    #[test]
    fn a_password_on_grant_is_refused_and_a_misspelt_span_is_named() {
        let Some(Command::Misread(message)) = parse("/grant sesion hunter2") else {
        // Not the buttons' form: a password with a `#` in front is still
        // a password in the wrong place, and comes back out of the chat.
        for typed in ["/grant #hunter2", "/grant always #pw", "/grant #ABC123"] {
            let command = parse(typed).unwrap();
            assert!(command.carries_a_secret(), "{typed}: {command:?}");
        }
        assert_eq!(
            parse("/deny because #tag"),
            Some(Command::Deny { ask: None })
        );
            panic!("a typo read as a password");
        };
        assert!(message.contains("sesion?"), "{message}");
        assert!(message.contains("/grant session"), "{message}");
        assert!(matches!(parse("/grant alwyas"), Some(Command::Misread(_))));
        assert!(matches!(parse("/grant onse"), Some(Command::Misread(_))));
        // Anything else after the span, or instead of it, is the
        // password — asked for on its own, after the yes.
        for typed in [
            "/grant hunter2",
            "/grant session hunter2",
            "/grant sessionkeyaddendum",
            "/grant abc",
        ] {
            let Some(Command::Misread(message)) = parse(typed) else {
                panic!("{typed} took a password");
            };
            // The exact text, not a lookalike: the gateway matches on
            // it to delete the message the password came in.
            assert_eq!(message, PASSWORD_AFTER_THE_YES, "{typed}");
            assert!(message.contains("/password <pw>"), "{typed}: {message}");
        }
        assert!(edits_within("sesion", "session", 2));
        assert!(!edits_within("hunter2", "session", 2));
        assert_eq!(parse("/pending"), Some(Command::Pending));
        // Bare is not "all": that is one tap on Telegram.
        assert_eq!(parse("/approve"), Some(Command::Approve(None)));
        assert_eq!(
            parse("/approve ab12"),
            Some(Command::Approve(Some("ab12".into())))
        );
        assert_eq!(
            parse("/reject all"),
            Some(Command::Reject(Some("all".into())))
        );
        // A phone that capitalises the first word is understood; an
        // unknown word is echoed as it was typed.
        assert_eq!(parse("/Help"), Some(Command::Help));
        assert_eq!(parse("/NEW"), Some(Command::New));
        assert_eq!(parse("/Stop"), Some(Command::Abort));
        assert_eq!(
            parse("/Model zai/glm-4.7"),
            Some(Command::Model {
                model: Some("zai/glm-4.7".into()),
                save: false
            })
        );
        assert_eq!(parse("/dance"), Some(Command::Unknown("dance".into())));
        assert_eq!(parse("/Dance"), Some(Command::Unknown("Dance".into())));
        assert_eq!(parse("/"), None);
        assert_eq!(parse("what about /new?"), None);
        assert_eq!(parse("1/2 done"), None);
    }

    /// A misspelt `/unlock` or `/password` unlocks nothing and leaves
    /// the password in the chat, so it is named as one: the gateway
    /// takes the message back out on `carries_a_secret`.
    #[test]
    fn a_mistyped_unlock_is_still_a_password_in_the_chat() {
        for (typed, meant) in [
            ("/unlok open sesame", "unlock"),
            ("/unlcok open sesame", "unlock"),
            ("/Unlokc open sesame", "unlock"),
            ("/pasword hunter2", "password"),
            ("/passwrod hunter2", "password"),
        ] {
            let Some(command) = parse(typed) else {
                panic!("{typed} did not parse");
            };
            assert_eq!(
                command,
                Command::MistypedSecret {
                    typed: typed[1..].split_whitespace().next().unwrap().to_string(),
                    meant,
                },
                "{typed}"
            );
            assert!(command.carries_a_secret(), "{typed}");
        }
        // Nothing after it is nothing to take back: an ordinary refusal
        // with the help under it, which names /unlock.
        assert_eq!(parse("/unlok"), Some(Command::Unknown("unlok".into())));
        assert!(!parse("/unlok").unwrap().carries_a_secret());
        // A word that is not trying to be either of them keeps its own
        // refusal, argument or no argument.
        assert_eq!(
            parse("/dance all night"),
            Some(Command::Unknown("dance".into()))
        );
        assert!(!parse("/dance all night").unwrap().carries_a_secret());
    }

    /// Every message with a password in it is taken back out, whichever
    /// way it was typed — the gateway asks the command, not the text.
    #[test]
    fn every_password_bearing_command_says_so() {
        for typed in [
            "/unlock open sesame",
            "/password hunter2",
            "/grant session hunter2",
            "/unlok open sesame",
        ] {
            assert!(parse(typed).unwrap().carries_a_secret(), "{typed}");
        }
        for typed in [
            "/unlock",
            "/password",
            "/grant session",
            "/new",
            "/help",
            "/dance",
        ] {
            assert!(!parse(typed).unwrap().carries_a_secret(), "{typed}");
        }
        // The misspelt span is named, not deleted: it is a typo of a
        // word, not a password.
        assert!(!parse("/grant sesion").unwrap().carries_a_secret());
    }

    /// Every alias the parser takes is a command a person can find.
    #[test]
    fn the_help_names_the_aliases_too() {
        assert!(HELP.contains("/abort (or /stop)"), "{HELP}");
        assert!(HELP.contains("/password <pw>"), "{HELP}");
        assert!(HELP.contains("/cost (or /usage)"), "{HELP}");
    }

    #[test]
    fn the_console_commands_parse() {
        assert_eq!(parse("/status"), Some(Command::Status));
        assert_eq!(parse("/cost"), Some(Command::Cost));
        assert_eq!(parse("/Usage"), Some(Command::Cost));
        assert_eq!(parse("/cron"), Some(Command::Cron { remove: None }));
        assert_eq!(
            parse("/cron remove ab12cd34"),
            Some(Command::Cron {
                remove: Some("ab12cd34".into())
            })
        );
        assert_eq!(
            parse("/cron Remove morning briefing"),
            Some(Command::Cron {
                remove: Some("morning briefing".into())
            })
        );
        assert_eq!(
            parse("/cron list"),
            Some(Command::Misread(CRON_USAGE.into()))
        );
        assert_eq!(
            parse("/cron remove"),
            Some(Command::Misread(CRON_USAGE.into()))
        );
        assert_eq!(parse("/tasks"), Some(Command::Tasks));
        assert_eq!(parse("/whoami"), Some(Command::Whoami));
        assert_eq!(parse("/restart"), Some(Command::Restart));
    }

    /// The menu and the help are one list: a command in either is in
    /// the other, every menu entry parses, and every entry fits what
    /// Telegram takes (lowercase names, descriptions under 256).
    #[test]
    fn the_menu_and_the_help_agree() {
        let in_help: std::collections::BTreeSet<&str> = HELP
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '/')
            .filter_map(|word| word.strip_prefix('/'))
            .filter(|name| !name.is_empty())
            .collect();
        let in_menu: std::collections::BTreeSet<&str> =
            MENU.iter().map(|(name, _)| *name).collect();
        // `/stop` and `/usage` are aliases; the menu carries one name each.
        let mut help_names = in_help.clone();
        help_names.remove("stop");
        help_names.remove("usage");
        assert_eq!(help_names, in_menu, "help {in_help:?} vs menu {in_menu:?}");
        for (name, description) in MENU {
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase()),
                "{name}: Telegram wants lowercase"
            );
            assert!(description.len() < 256, "{name}");
            let parsed = parse(&format!("/{name}")).expect("parses");
            assert!(
                !matches!(parsed, Command::Unknown(_)),
                "/{name} is unknown to the parser"
            );
        }
    }
}
