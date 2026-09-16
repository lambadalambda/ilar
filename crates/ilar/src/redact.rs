//! Hiding secrets in text that is about to be shown or logged.
//!
//! Two surfaces need this and they need the same answers: a tool row,
//! which shows the model's own arguments back to the person, and a
//! provider's error body, which is untrusted text that routinely
//! quotes the request that failed. They had a copy each — same idea,
//! different needle lists, different quote rules — and the drift was
//! not academic: one list was missing `privatekey`, so a provider
//! error naming one published it, and `service` commands were once
//! published verbatim where `bash` commands were redacted.
//!
//! So: one needle table, one token pass, one URL rule. What is *not*
//! shared is policy — which values a surface runs this over. A tool
//! argument is the model's own words and only named things are
//! rewritten; an error body is a stranger's and every string in it
//! goes through the token pass. Those live with their callers.
//!
//! Display-side throughout. The persisted event and the provider
//! request keep the raw text; nothing here is a security boundary, it
//! is what keeps a secret off a screen and out of a log.

/// What replaces a secret, everywhere.
pub(crate) const REDACTED: &str = "<redacted>";

/// A key name reduced to its letters and digits, lowercased, so
/// `api_key`, `apiKey` and `API-KEY` are one name.
pub(crate) fn normalized_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether a name says its value is a secret. One table: a needle
/// added here is added to every surface at once, which is the whole
/// point of the module.
///
/// Matched as a substring, deliberately — `x-auth-token`, `apiKeyId`
/// and `aws_secret_access_key` are all secrets and none of them is a
/// bare needle. The cost is that `tokens_used` reads as one and a
/// count is shown as `<redacted>`, which is nobody's loss next to the
/// other mistake.
pub(crate) fn sensitive_key(key: &str) -> bool {
    let normalized = normalized_key(key);
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "privatekey",
        "credential",
        "cookie",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

/// How freely the token pass arms itself, which is the one thing the
/// two surfaces genuinely disagree about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A command line somebody typed or a model wrote. A bare word
    /// that happens to be sensitive does not arm the next token:
    /// `grep token notes.txt` would otherwise hide the filename. A
    /// secret in a command announces itself as a flag, a header or an
    /// assignment.
    Command,
    /// Prose from somewhere else — a provider's error body. Nothing
    /// here is worth showing at the cost of missing a secret, so a
    /// bare sensitive word arms the next token too.
    Untrusted,
}

/// Quotes and punctuation a token may be wrapped in. Braces because an
/// error body is often minified JSON; quotes and commas because both
/// surfaces carry them.
const WRAPPERS: [char; 5] = ['\'', '"', ',', '{', '}'];

/// One whitespace-separated pass over `text`, hiding the tokens that
/// follow a sensitive flag or header and the ones that announce
/// themselves. Every value it hides is pushed to `secrets`, for the
/// caller that then has to find those same values echoed back in a
/// result.
///
/// Whitespace is normalised to single spaces on the way out: both
/// callers show the result rather than run it.
pub(crate) fn tokens(text: &str, mode: Mode, secrets: &mut Vec<String>) -> String {
    let mut redact_next = false;
    text.split_whitespace()
        .map(|token| {
            let bare = token.trim_matches(WRAPPERS);
            if redact_next {
                // The scheme word is not the credential; the token
                // after it is. Stays armed.
                if is_scheme(bare) || bare == "=" || bare == ":" {
                    return token.to_string();
                }
                redact_next = false;
                // Never an empty needle: both callers strike what they
                // collect out of a result, and an empty one strikes
                // everything.
                if !bare.is_empty() {
                    secrets.push(bare.to_string());
                }
                return REDACTED.to_string();
            }
            // Keys that name themselves, whatever they sit beside.
            if bare.starts_with("sk-")
                || bare.starts_with("ghp_")
                || bare.starts_with("github_pat_")
            {
                secrets.push(bare.to_string());
                return REDACTED.to_string();
            }
            let lower = bare.to_ascii_lowercase();
            // `Authorization:` carries its value in the same token as
            // often as in the next one, and the header name is worth
            // keeping — it says what was hidden.
            if let Some(position) = lower.find("authorization:") {
                let value = lower[position + "authorization:".len()..].trim();
                if value.is_empty() || value == "bearer" || value == "basic" {
                    redact_next = true;
                    return token.to_string();
                }
                // Sliced from the original, not the lowercased copy:
                // the secret has to match its own casing to be found
                // in an echo later.
                secrets.push(bare[position + "authorization:".len()..].trim().to_string());
                return format!("{}{REDACTED}", &bare[..position + "authorization:".len()]);
            }
            if let Some((key, value, separator)) = split_assignment(bare, mode)
                && sensitive_key(key)
            {
                // A scheme word is not the value, it announces it.
                if value.is_empty() || is_scheme(value.trim_matches(WRAPPERS)) {
                    redact_next = true;
                    return token.to_string();
                }
                let value = value.trim_matches(WRAPPERS);
                if !value.is_empty() {
                    secrets.push(value.to_string());
                }
                return format!("{key}{separator}{REDACTED}");
            }
            // A bare name whose value is the next token. `bearer`
            // announces one on any surface; `basic` and the plain
            // names are ordinary English — `echo basic auth test`
            // would lose its words — so they arm only where
            // over-hiding is the cheaper mistake.
            if bare.eq_ignore_ascii_case("bearer")
                || (mode == Mode::Untrusted && (is_scheme(bare) || sensitive_key(bare)))
            {
                redact_next = true;
                return token.to_string();
            }
            url_credentials(token).unwrap_or_else(|| token.to_string())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--token=x`, `TOKEN=x`, `"api_key":"x"`, `--token` — the name, what
/// followed it (empty when the value is in the next token), and the
/// separator to write back. `None` when the token is not a name at all.
///
/// A colon is a separator in a header and in minified JSON, and it is
/// also how a command line writes a *location*: `src/token.rs:88:3`,
/// `tests/test_token.py::test_login`, `-v ~/.password-store:/data`.
/// Reading those as secrets does not only blank a path in the row — the
/// value it "hides" is collected and then struck out of the tool's
/// whole output. So in a command a colon key that looks like a path
/// is not a name; in an untrusted body, where nothing is worth
/// missing a secret over, every colon splits.
fn split_assignment(bare: &str, mode: Mode) -> Option<(&str, &str, &str)> {
    if let Some((key, value)) = bare.split_once('=') {
        return Some((key, value, "="));
    }
    if let Some((key, value)) = bare.split_once(':')
        && (mode == Mode::Untrusted
            || value.is_empty()
            || bare.starts_with('-')
            || !key.contains(['/', '.']))
    {
        return Some((key, value, ":"));
    }
    bare.starts_with('-').then_some((bare, "", ""))
}

/// An authentication scheme: the word in front of the credential, not
/// the credential.
fn is_scheme(token: &str) -> bool {
    token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("basic")
}

/// `scheme://user:secret@host` with the credential replaced — the whole
/// userinfo, since the username is usually the account the secret
/// opens. `None` when nothing changed. Requires `://` directly ahead of
/// the credential, so an email or a bare `user:pass@host` in prose is
/// never touched, and a plain URL has no `user:secret@` to match.
pub(crate) fn url_credentials(text: &str) -> Option<String> {
    let mut redacted = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    while let Some(position) = rest.find("://") {
        let after = position + "://".len();
        redacted.push_str(&rest[..after]);
        rest = &rest[after..];
        // The authority ends where a path, query, fragment or plain
        // prose begins — and at any character a URL authority cannot
        // contain. Minified JSON is one whitespace-free run: without
        // the delimiter set, `{"host":"https://api.io","user":"bob@x"}`
        // reads `api.io","user":"bob` as userinfo and this pass would
        // corrupt the value and invent a credential.
        let authority_end = rest
            .find(|c: char| {
                matches!(
                    c,
                    '/' | '?'
                        | '#'
                        | '"'
                        | '\''
                        | ','
                        | ';'
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | '('
                        | ')'
                        | '<'
                        | '>'
                        | '\\'
                        | '`'
                ) || c.is_whitespace()
            })
            .unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@')
            && let Some((user, secret)) = rest[..at].split_once(':')
            && !user.is_empty()
            && !secret.is_empty()
        {
            redacted.push_str(REDACTED);
            redacted.push('@');
            rest = &rest[at + 1..];
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    redacted.push_str(rest);
    Some(redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hidden(text: &str, mode: Mode) -> String {
        tokens(text, mode, &mut Vec::new())
    }

    /// The table both surfaces read. `privatekey` is here because one
    /// copy had it and the other did not, which is how a provider
    /// error naming a private key was published.
    #[test]
    fn one_needle_table_answers_for_every_surface() {
        for key in [
            "token",
            "api_key",
            "apiKey",
            "API-KEY",
            "private_key",
            "privateKey",
            "x-auth-token",
            "Authorization",
            "password",
            "set-cookie",
            "aws_secret_access_key",
            "credentials",
        ] {
            assert!(sensitive_key(key), "{key} names a secret");
        }
        for key in ["path", "command", "name", "model", "cook"] {
            assert!(!sensitive_key(key), "{key} does not");
        }
        // The substring rule's price, paid on purpose: a count reads
        // as a secret, and a count is a cheap thing to lose.
        assert!(sensitive_key("tokens_used"));
    }

    /// The shapes a secret arrives in, hidden the same way whichever
    /// surface is asking.
    #[test]
    fn every_shape_of_secret_is_hidden_on_both_surfaces() {
        for mode in [Mode::Command, Mode::Untrusted] {
            for (text, expected) in [
                ("--token=hunter2", "--token=<redacted>"),
                ("--token hunter2", "--token <redacted>"),
                ("API_KEY=hunter2 make", "API_KEY=<redacted> make"),
                (
                    "-H 'Authorization: Bearer hunter2'",
                    "-H 'Authorization: Bearer <redacted>",
                ),
                ("Authorization:hunter2", "Authorization:<redacted>"),
                ("sk-abcdefghijklmnop", "<redacted>"),
                ("ghp_abcdefghijklmnop", "<redacted>"),
                (
                    "https://bob:hunter2@api.example.com/v1",
                    "https://<redacted>@api.example.com/v1",
                ),
            ] {
                let got = hidden(text, mode);
                assert!(
                    got.starts_with(expected),
                    "{mode:?} on {text:?}: got {got:?}, wanted {expected:?}"
                );
                assert!(!got.contains("hunter2"), "{mode:?} on {text:?}: {got}");
            }
        }
    }

    /// The one difference, and the reason for it: in a command a bare
    /// sensitive word is usually a filename's neighbour, and hiding
    /// what follows it would hide the file. In a stranger's error body
    /// nothing is worth that risk.
    #[test]
    fn a_bare_word_arms_only_where_over_hiding_is_cheap() {
        assert_eq!(
            hidden("grep token notes.txt", Mode::Command),
            "grep token notes.txt"
        );
        assert_eq!(hidden("token hunter2", Mode::Untrusted), "token <redacted>");
        assert_eq!(
            hidden("echo basic auth test", Mode::Command),
            "echo basic auth test",
            "a common English word must not swallow the word after it"
        );
        // And a name that is not the key of an assignment still arms
        // in a body: a key that failed the sensitive test must not
        // shadow the whole-token one.
        assert_eq!(
            hidden("//registry.npmjs.org/:_authToken hunter2", Mode::Untrusted),
            "//registry.npmjs.org/:_authToken <redacted>"
        );
        assert_eq!(
            hidden("code=invalid_api_key hunter2", Mode::Untrusted),
            "code=invalid_api_key <redacted>"
        );
    }

    /// A colon is a header's separator, JSON's separator — and how a
    /// command line writes a location. Reading a path as a secret
    /// blanks it in the row *and* strikes the value out of the tool's
    /// whole output, which is how an over-eager rule becomes a lost
    /// test report.
    #[test]
    fn a_path_with_a_colon_is_not_a_secret_in_a_command() {
        for command in [
            "code src/token.rs:88:3",
            "pytest tests/test_token.py::test_login -v",
            "docker run -v /home/u/.password-store:/data img",
            "rg -n TODO src/secrets.rs:12",
        ] {
            let mut secrets = Vec::new();
            assert_eq!(
                tokens(command, Mode::Command, &mut secrets),
                command,
                "a location is not a credential"
            );
            assert!(secrets.is_empty(), "{command}: {secrets:?}");
        }
        // The shapes that *are* names keep working, colon and all.
        assert_eq!(
            hidden("curl -H X-Api-Key:hunter2 https://x.io", Mode::Command),
            "curl -H X-Api-Key:<redacted> https://x.io"
        );
        assert_eq!(
            hidden("mytool password: hunter2", Mode::Command),
            "mytool password: <redacted>"
        );
    }

    /// The armed state survives what sits between a name and its
    /// value — an equals sign, a scheme word — instead of spending
    /// itself on them and leaving the credential in the clear.
    #[test]
    fn arming_survives_the_punctuation_between_name_and_value() {
        assert_eq!(
            hidden("--token = hunter2", Mode::Command),
            "--token = <redacted>"
        );
        assert_eq!(
            hidden("--authorization Bearer hunter2", Mode::Command),
            "--authorization Bearer <redacted>"
        );
        // A scheme word as the value announces the next token too.
        assert_eq!(
            hidden(r#"{"authorization":"Bearer hunter2"}"#, Mode::Untrusted),
            r#"{"authorization":"Bearer <redacted>"#
        );
    }

    /// Byte offsets from a lowercased copy, used against the original:
    /// the slice has to land on a character boundary whatever else is
    /// in the token.
    #[test]
    fn a_header_survives_the_letters_around_it() {
        for token in [
            "ПРИВЕТAuthorization:hunter2",
            "Authorization:hüñter2",
            "X-Authorization:hunter2",
        ] {
            let got = hidden(token, Mode::Untrusted);
            assert!(got.ends_with("<redacted>"), "{token}: {got}");
            assert!(!got.contains("hunter2"), "{token}: {got}");
        }
        // The header keeps its own name and casing, which says what
        // was hidden.
        assert_eq!(
            hidden("X-Authorization:hunter2", Mode::Command),
            "X-Authorization:<redacted>"
        );
    }

    /// What it hides, it reports — the caller that has to find the
    /// same value echoed back in a result cannot ask twice.
    #[test]
    fn hidden_values_are_collected_for_the_echo_hunt() {
        let mut secrets = Vec::new();
        tokens(
            "curl -H 'Authorization: Bearer hunter2' --api-key swordfish https://x.io",
            Mode::Command,
            &mut secrets,
        );
        assert!(
            secrets.iter().any(|secret| secret == "hunter2"),
            "{secrets:?}"
        );
        assert!(
            secrets.iter().any(|secret| secret.contains("swordfish")),
            "{secrets:?}"
        );
    }

    /// A URL credential is a secret under any key and in any prose,
    /// which is why no name predicate catches it — and why the error
    /// body, which had no such pass at all, published them.
    #[test]
    fn a_credentialed_url_is_hidden_wherever_it_appears() {
        assert_eq!(
            url_credentials("git+https://bob:hunter2@host/repo.git").as_deref(),
            Some("git+https://<redacted>@host/repo.git")
        );
        assert_eq!(url_credentials("https://api.example.com/v1"), None);
        assert_eq!(url_credentials("mailto:bob:x@example.com"), None);
        // Minified JSON is one token: the authority must not run past
        // the quote and invent a credential out of the next field.
        assert_eq!(
            url_credentials(r#"{"host":"https://api.io","user":"bob@x"}"#),
            None
        );
    }
}
