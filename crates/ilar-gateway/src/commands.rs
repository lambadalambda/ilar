//! Slash commands a person types into the chat. Handled by the gateway
//! itself, before any model is involved.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Start over: a fresh session for this chat. Memory stays.
    New,
    /// List the models, or switch to one.
    Model(Option<String>),
    Help,
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
        ("model", argument) => Command::Model(argument.map(str::to_string)),
        ("help", _) => Command::Help,
        (other, _) => Command::Unknown(other.to_string()),
    })
}

pub const HELP: &str = "/new — start a fresh chat (memory stays)\n\
/model — list the models; /model <provider/model> switches\n\
/help — this";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_and_prose_does_not() {
        assert_eq!(parse("/new"), Some(Command::New));
        assert_eq!(parse("  /new please"), Some(Command::New));
        assert_eq!(parse("/model"), Some(Command::Model(None)));
        assert_eq!(
            parse("/model zai/glm-4.7"),
            Some(Command::Model(Some("zai/glm-4.7".into())))
        );
        assert_eq!(parse("/model   "), Some(Command::Model(None)));
        assert_eq!(parse("/help"), Some(Command::Help));
        assert_eq!(parse("/dance"), Some(Command::Unknown("dance".into())));
        assert_eq!(parse("/"), None);
        assert_eq!(parse("what about /new?"), None);
        assert_eq!(parse("1/2 done"), None);
    }
}
