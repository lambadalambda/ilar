//! `ilar secret …`: the store's command line. A value is asked for
//! hidden at a terminal and read from stdin when piped, never taken as
//! an argument, so it stays out of shell history and process listings.

use anyhow::{Context, Result};
use ilar::secrets::SecretStore;

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum SecretCommand {
    /// Store a secret; at a terminal the value is asked for hidden and
    /// confirmed, piped it is read from stdin
    Set {
        /// Environment-variable style name, like GITHUB_TOKEN
        name: String,
        /// What it is for, shown to the model in place of the value
        #[arg(long, default_value = "")]
        description: String,
    },
    /// Names, descriptions and standing grants; never values
    List,
    /// Forget a secret and its grants
    Remove {
        /// The stored secret's name
        name: String,
    },
    /// Let a tool use a secret without asking, for good
    Grant {
        /// The stored secret's name, or root for the sudo tool
        name: String,
        /// The tool that may use it: bash, service, or sudo for root
        #[arg(long)]
        tool: String,
    },
    /// Drop standing grants, for one tool or all of them
    Revoke {
        /// The stored secret's name, or root
        name: String,
        /// One tool to drop; every tool when left out
        #[arg(long)]
        tool: Option<String>,
    },
    /// Seal the store under a master password, asked for once per session from then on
    Encrypt,
    /// Write the store back in the clear
    Decrypt,
}

/// How a master password is asked for: hidden input on the terminal
/// outside tests. The prompt text is the argument.
pub(crate) type AskPassword<'a> = &'a mut dyn FnMut(&str) -> Result<String>;

/// The terminal's own hidden prompt.
pub(crate) fn ask_on_terminal(prompt: &str) -> Result<String> {
    if !terminal_is_answerable() {
        anyhow::bail!(
            "reading the master password: this job is in the background, where the terminal stops \
             it rather than let it ask — run `fg` and start again"
        );
    }
    rpassword::prompt_password(prompt).context("reading the master password")
}

/// Whether a prompt on the controlling terminal can actually be
/// answered. A job backgrounded from a shell keeps `/dev/tty` open, so
/// the terminal looks reachable — but a background process group that
/// touches it is stopped (`SIGTTOU` for the mode change the hidden
/// prompt makes, `SIGTTIN` for the read behind it). No prompt on
/// screen, nothing to type into, and a process that looks hung. Which
/// process group owns the terminal is the whole test.
///
/// `foreground` is `tcgetpgrp` of the controlling terminal and `ours`
/// is `getpgrp()`. A negative `foreground` means the terminal answered
/// no owner, which is not this problem: the read then fails with a
/// message instead of blocking, and the caller carries on locked.
#[cfg(unix)]
fn prompt_can_be_answered(foreground: i32, ours: i32) -> bool {
    foreground < 0 || foreground == ours
}

#[cfg(unix)]
fn terminal_is_answerable() -> bool {
    use std::os::fd::AsRawFd;
    // `/dev/tty`, not stdin: that is the file rpassword opens, and
    // stdin answers a different question. `echo … | ilar secret set`
    // redirects stdin and is not a backgrounded job; a backgrounded
    // job may well have stdin on the terminal still.
    let Ok(tty) = std::fs::File::open("/dev/tty") else {
        // No controlling terminal at all — cron, systemd, a container.
        // The read fails there with a message of its own.
        return true;
    };
    // SAFETY: both calls only read process and terminal state, take no
    // pointers, and `tty` outlives the call.
    let foreground = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
    let ours = unsafe { libc::getpgrp() };
    prompt_can_be_answered(foreground, ours)
}

#[cfg(not(unix))]
fn terminal_is_answerable() -> bool {
    true
}

/// What a driver about to take over the terminal asks: an empty answer
/// is a decision, so it is offered.
pub(crate) const STARTUP_PROMPT: &str = "Secret store master password (Enter leaves it locked): ";
/// What `ilar secret …` asks: it has nothing to do with the store
/// locked, so leaving it locked is not offered as a choice.
const CLI_PROMPT: &str = "Secret store master password: ";
/// Tries a driver that carries on regardless gives a typo.
pub(crate) const STARTUP_TRIES: usize = 3;

/// Unlock a sealed store for this process, asking up to `tries` times.
/// An empty answer leaves it locked, and so does the last wrong one
/// when the caller allowed more than a single try — it means to run
/// locked rather than exit. With one try, a wrong password is the
/// error it is.
pub(crate) fn unlock_if_sealed(
    store: &SecretStore,
    prompt: &str,
    tries: usize,
    ask: AskPassword<'_>,
) -> Result<bool> {
    if !store.is_locked() {
        return Ok(true);
    }
    for asked in 1..=tries.max(1) {
        let password = if asked == 1 {
            ask(prompt)?
        } else {
            ask(&format!("Wrong master password. {prompt}"))?
        };
        if password.trim().is_empty() {
            return Ok(false);
        }
        match store.unlock(&password) {
            Ok(()) => return Ok(true),
            Err(error) if asked >= tries.max(1) => {
                return if tries > 1 { Ok(false) } else { Err(error) };
            }
            Err(_) => {}
        }
    }
    Ok(false)
}

/// The names the store holds, without the `root` row: root is not
/// stored, and a line that lists it among stored names suggests
/// `ilar secret set root`, which is refused.
fn stored_names(store: &SecretStore) -> Result<Vec<String>> {
    Ok(store
        .list()?
        .into_iter()
        .map(|secret| secret.name)
        .filter(|name| name != ilar::secrets::ROOT)
        .collect())
}

/// A name the store does not have is an error, whatever the command:
/// exit 0 on a typo reads as "done". A store that cannot be read is
/// reported by the command that read it, not guessed at here.
fn unknown_name(store: &SecretStore, name: &str) -> anyhow::Error {
    let names = stored_names(store).unwrap_or_default();
    if names.is_empty() {
        anyhow::anyhow!("no secret named {name}; the store is empty")
    } else {
        anyhow::anyhow!("no secret named {name}; stored: {}", names.join(", "))
    }
}

/// Whether a grant could ever be read: `root` is the sudo tool's
/// pseudo-secret and sudo takes nothing else, so the other two pairings
/// would sit in the store looking effective and do nothing.
fn grantable(name: &str, tool: &str) -> Result<()> {
    if !ilar::secrets::GRANTABLE_TOOLS.contains(&tool) {
        anyhow::bail!(
            "no tool named {tool} takes secrets; one of: {}",
            ilar::secrets::GRANTABLE_TOOLS.join(", ")
        );
    }
    match (name == ilar::secrets::ROOT, tool == "sudo") {
        (true, false) => {
            anyhow::bail!("only the sudo tool asks for root; grant it with --tool sudo")
        }
        (false, true) => anyhow::bail!(
            "the sudo tool takes no secrets, only root; {name} goes to bash or service"
        ),
        _ => Ok(()),
    }
}

/// Run one command against the store; the text is what to print.
/// `piped` is stdin when it is not a terminal: `set` reads the value
/// from it. At a terminal (`None`) the value is asked for hidden, and
/// asked again to confirm, like the master password.
pub(crate) fn run(
    store: &SecretStore,
    command: SecretCommand,
    piped: Option<&mut dyn std::io::Read>,
    ask: AskPassword<'_>,
) -> Result<String> {
    // Before the master password is asked for: a name the store would
    // refuse anyway costs nothing to type twice.
    if let SecretCommand::Set { name, .. } = &command {
        ilar::secrets::valid_name(name).map_err(anyhow::Error::msg)?;
    }
    match &command {
        SecretCommand::Encrypt => {
            if store.is_sealed() {
                anyhow::bail!(
                    "the store is already sealed; decrypt it first to change the password"
                );
            }
            let password = ask("New master password: ")?;
            // Checked before the confirmation: typing a password twice
            // to be told it was too short the first time is a waste.
            if password.chars().count() < ilar::secrets::MIN_VALUE_CHARS {
                anyhow::bail!(
                    "a master password is at least {} characters; nothing sealed",
                    ilar::secrets::MIN_VALUE_CHARS
                );
            }
            if password != ask("Again: ")? {
                anyhow::bail!("the two did not match; nothing sealed");
            }
            store.encrypt(&password)?;
            return Ok(format!(
                "Sealed {}; every session asks for the master password once",
                store.path().display()
            ));
        }
        SecretCommand::Decrypt => {
            if !store.is_sealed() {
                anyhow::bail!("the store is not sealed");
            }
            store.decrypt(&ask("Master password: ")?)?;
            return Ok(format!("{} is in the clear again", store.path().display()));
        }
        _ => {
            if !unlock_if_sealed(store, CLI_PROMPT, 1, ask)? {
                anyhow::bail!(
                    "the store is sealed; nothing can be read or written without the master password"
                );
            }
        }
    }
    match command {
        SecretCommand::Set { name, description } => {
            let value = match piped {
                Some(stdin) => {
                    let mut value = String::new();
                    stdin
                        .read_to_string(&mut value)
                        .context("reading the value from stdin")?;
                    let value = value.trim_end_matches(['\n', '\r']).to_string();
                    if value.is_empty() {
                        anyhow::bail!("no value on stdin");
                    }
                    value
                }
                None => {
                    let value = ask(&format!("Value for {name}: "))?;
                    if value.is_empty() {
                        anyhow::bail!("no value given");
                    }
                    if value != ask("Again: ")? {
                        anyhow::bail!("the two did not match; nothing stored");
                    }
                    value
                }
            };
            let replaced = store.set(&name, &description, &value)?;
            Ok(format!(
                "{} {name} in {}",
                if replaced { "Replaced" } else { "Stored" },
                store.path().display()
            ))
        }
        SecretCommand::List => {
            let listed = store.list()?;
            if listed.is_empty() {
                return Ok(format!(
                    "No secrets stored in {}. Add one with: ilar secret set NAME",
                    store.path().display()
                ));
            }
            Ok(listed
                .iter()
                .map(|secret| {
                    // The separator the model's own listing uses, so one
                    // reads like the other.
                    let mut line = secret.name.clone();
                    if !secret.description.is_empty() {
                        line.push_str(&format!(" — {}", secret.description));
                    }
                    if !secret.always.is_empty() {
                        line.push_str(&format!(" [always: {}]", secret.always.join(", ")));
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        SecretCommand::Remove { name } => {
            if name == ilar::secrets::ROOT {
                anyhow::bail!(
                    "{root} is not stored: it is what the sudo tool asks for. \
                     `ilar secret revoke {root}` drops its standing approval",
                    root = ilar::secrets::ROOT
                );
            }
            if !store.remove(&name)? {
                return Err(unknown_name(store, &name));
            }
            Ok(format!("Removed {name}"))
        }
        SecretCommand::Grant { name, tool } => {
            grantable(&name, &tool)?;
            if !store.grant_always(&name, &tool)? {
                return Err(unknown_name(store, &name));
            }
            Ok(format!("{tool} may use {name} without asking"))
        }
        SecretCommand::Revoke { name, tool } => {
            if let Some(tool) = tool.as_deref() {
                grantable(&name, tool)?;
            }
            // Told apart from a name with nothing to revoke: one is a
            // typo, the other is already the way it was asked for. The
            // listing is read here rather than guessed at, so a store
            // that cannot be read says that instead of "no such name".
            let names = stored_names(store)?;
            if name != ilar::secrets::ROOT && !names.contains(&name) {
                return Err(unknown_name(store, &name));
            }
            Ok(if store.revoke(&name, tool.as_deref())? {
                match tool {
                    Some(tool) => format!("{tool} will ask for {name} again"),
                    None => format!("Every tool will ask for {name} again"),
                }
            } else {
                format!("Nothing to revoke for {name}")
            })
        }
        SecretCommand::Encrypt | SecretCommand::Decrypt => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_password(_: &str) -> Result<String> {
        panic!("a plain store asks for no master password")
    }

    /// One command on a plain store, with nothing on stdin and nobody
    /// to ask: what every command but `set` needs.
    fn dry(store: &SecretStore, command: SecretCommand) -> Result<String> {
        let mut none: &[u8] = b"";
        run(store, command, Some(&mut none), &mut no_password)
    }

    #[test]
    fn the_value_comes_from_stdin_and_never_shows_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path());
        let mut stdin: &[u8] = b"ghp_secret\n";
        let out = run(
            &store,
            SecretCommand::Set {
                name: "GITHUB_TOKEN".into(),
                description: "for gh".into(),
            },
            Some(&mut stdin),
            &mut no_password,
        )
        .unwrap();
        assert!(out.starts_with("Stored GITHUB_TOKEN"), "{out}");
        let mut empty: &[u8] = b"\n";
        assert!(
            run(
                &store,
                SecretCommand::Set {
                    name: "X".into(),
                    description: String::new()
                },
                Some(&mut empty),
                &mut no_password
            )
            .is_err()
        );
        let mut none: &[u8] = b"";
        let out = run(
            &store,
            SecretCommand::Grant {
                name: "GITHUB_TOKEN".into(),
                tool: "bash".into(),
            },
            Some(&mut none),
            &mut no_password,
        )
        .unwrap();
        assert_eq!(out, "bash may use GITHUB_TOKEN without asking");
        assert!(
            run(
                &store,
                SecretCommand::Grant {
                    name: "GITHUB_TOKEN".into(),
                    tool: "read".into()
                },
                Some(&mut none),
                &mut no_password
            )
            .is_err()
        );
        let out = run(
            &store,
            SecretCommand::List,
            Some(&mut none),
            &mut no_password,
        )
        .unwrap();
        assert_eq!(out, "GITHUB_TOKEN — for gh [always: bash]");
        assert!(!out.contains("ghp_secret"));
        let out = run(
            &store,
            SecretCommand::Revoke {
                name: "GITHUB_TOKEN".into(),
                tool: None,
            },
            Some(&mut none),
            &mut no_password,
        )
        .unwrap();
        assert_eq!(out, "Every tool will ask for GITHUB_TOKEN again");
        let out = run(
            &store,
            SecretCommand::Remove {
                name: "GITHUB_TOKEN".into(),
            },
            Some(&mut none),
            &mut no_password,
        )
        .unwrap();
        assert_eq!(out, "Removed GITHUB_TOKEN");
        assert!(
            run(
                &store,
                SecretCommand::List,
                Some(&mut none),
                &mut no_password
            )
            .unwrap()
            .starts_with("No secrets stored")
        );
    }

    /// Every command fails the same way on a name the store does not
    /// have, `root` is explained rather than reported missing, and a
    /// grant nothing would ever read is refused.
    #[test]
    fn a_name_the_store_does_not_have_is_an_error_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path());
        let failure = |result: Result<String>| result.unwrap_err().to_string();

        // An empty store says where it is and what to type.
        let out = dry(&store, SecretCommand::List).unwrap();
        assert!(out.starts_with("No secrets stored in "), "{out}");
        assert!(out.ends_with("ilar secret set NAME"), "{out}");

        let missing = failure(dry(
            &store,
            SecretCommand::Remove {
                name: "NOPE".into(),
            },
        ));
        assert!(missing.contains("no secret named NOPE"), "{missing}");
        assert!(missing.contains("the store is empty"), "{missing}");
        store.set("KEY", "", "value-one").unwrap();
        for command in [
            SecretCommand::Remove {
                name: "NOPE".into(),
            },
            SecretCommand::Grant {
                name: "NOPE".into(),
                tool: "bash".into(),
            },
            SecretCommand::Revoke {
                name: "NOPE".into(),
                tool: None,
            },
        ] {
            let missing = failure(dry(&store, command));
            assert!(missing.contains("stored: KEY"), "{missing}");
        }
        // A real name with nothing to revoke is not a typo.
        let out = dry(
            &store,
            SecretCommand::Revoke {
                name: "KEY".into(),
                tool: None,
            },
        )
        .unwrap();
        assert_eq!(out, "Nothing to revoke for KEY");

        // root is not stored, and says what does drop it.
        let root = failure(dry(
            &store,
            SecretCommand::Remove {
                name: ilar::secrets::ROOT.into(),
            },
        ));
        assert!(root.contains("is not stored"), "{root}");
        assert!(root.contains("ilar secret revoke root"), "{root}");

        // Grants nothing reads: root to anything but sudo, sudo
        // anything but root, and a tool that takes no secrets at all.
        let pairs = [
            (ilar::secrets::ROOT, "bash", "grant it with --tool sudo"),
            ("KEY", "sudo", "takes no secrets"),
            ("KEY", "read", "no tool named read"),
        ];
        for (name, tool, says) in pairs {
            let refused = failure(dry(
                &store,
                SecretCommand::Grant {
                    name: name.into(),
                    tool: tool.into(),
                },
            ));
            assert!(refused.contains(says), "{refused}");
            let refused = failure(dry(
                &store,
                SecretCommand::Revoke {
                    name: name.into(),
                    tool: Some(tool.into()),
                },
            ));
            assert!(refused.contains(says), "{refused}");
        }
        let out = dry(
            &store,
            SecretCommand::Grant {
                name: ilar::secrets::ROOT.into(),
                tool: "sudo".into(),
            },
        )
        .unwrap();
        assert_eq!(out, "sudo may use root without asking");
        let out = dry(
            &store,
            SecretCommand::Revoke {
                name: ilar::secrets::ROOT.into(),
                tool: None,
            },
        )
        .unwrap();
        assert_eq!(out, "Every tool will ask for root again");
    }

    /// At a terminal the value is typed hidden and confirmed, never
    /// echoed and never taken from the terminal's raw input.
    #[test]
    fn at_a_terminal_the_value_is_asked_for_twice() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path());
        let set = || SecretCommand::Set {
            name: "TOKEN".into(),
            description: String::new(),
        };
        let mut prompts = Vec::new();
        let mut answers = vec!["first-try".to_string(), "second-try".to_string()];
        let mut mismatch = |prompt: &str| {
            prompts.push(prompt.to_string());
            Ok(answers.remove(0))
        };
        let error = run(&store, set(), None, &mut mismatch).unwrap_err();
        assert!(error.to_string().contains("did not match"), "{error}");
        assert_eq!(prompts, ["Value for TOKEN: ", "Again: "]);
        assert!(store.list().unwrap().is_empty());
        let mut agree = |_: &str| Ok("same-value".to_string());
        let out = run(&store, set(), None, &mut agree).unwrap();
        assert!(out.starts_with("Stored TOKEN"), "{out}");
        assert_eq!(store.value("TOKEN").unwrap().as_deref(), Some("same-value"));
    }

    /// Sealing asks twice; afterwards every command asks once, and an
    /// empty answer is a refusal to work blind.
    #[test]
    fn a_sealed_store_asks_for_its_master_password() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path());
        let mut stdin: &[u8] = b"ghp_secret\n";
        run(
            &store,
            SecretCommand::Set {
                name: "GITHUB_TOKEN".into(),
                description: String::new(),
            },
            Some(&mut stdin),
            &mut no_password,
        )
        .unwrap();
        let mut none: &[u8] = b"";
        // A master password under the floor is refused after one ask:
        // typing it twice to hear it was too short is a waste.
        let mut asked = 0;
        let mut short = |_: &str| {
            asked += 1;
            Ok("abc".to_string())
        };
        let error = run(&store, SecretCommand::Encrypt, Some(&mut none), &mut short).unwrap_err();
        assert!(error.to_string().contains("nothing sealed"), "{error}");
        assert_eq!(asked, 1);
        let mut answers = vec!["open sesame".to_string(), "open sesame".to_string()];
        let mut ask = |_: &str| Ok(answers.remove(0));
        let out = run(&store, SecretCommand::Encrypt, Some(&mut none), &mut ask).unwrap();
        assert!(out.starts_with("Sealed "), "{out}");
        assert!(store.is_sealed());
        // Forget it, as a new process would.
        ilar::secrets::forget_master(&store);
        assert!(store.is_locked());
        // A name the store would refuse anyway is refused before the
        // master password is asked for.
        let error = run(
            &store,
            SecretCommand::Set {
                name: "1bad".into(),
                description: String::new(),
            },
            Some(&mut none),
            &mut no_password,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("is not a secret name"),
            "{error}"
        );
        let mut refuse = |_: &str| Ok(String::new());
        assert!(run(&store, SecretCommand::List, Some(&mut none), &mut refuse).is_err());
        let mut wrong = |_: &str| Ok("nope".to_string());
        assert!(run(&store, SecretCommand::List, Some(&mut none), &mut wrong).is_err());
        let mut right = |_: &str| Ok("open sesame".to_string());
        let out = run(&store, SecretCommand::List, Some(&mut none), &mut right).unwrap();
        assert_eq!(out, "GITHUB_TOKEN");
        // Held now: no further asking.
        let out = run(
            &store,
            SecretCommand::List,
            Some(&mut none),
            &mut no_password,
        )
        .unwrap();
        assert_eq!(out, "GITHUB_TOKEN");
        let out = run(&store, SecretCommand::Decrypt, Some(&mut none), &mut right).unwrap();
        assert!(out.contains("in the clear"), "{out}");
    }

    /// The startup prompt a driver puts up forgives typos — three
    /// tries, then it runs locked — and never exits over one.
    #[test]
    fn the_startup_prompt_re_asks_and_then_runs_locked() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(dir.path());
        store.set("KEY", "", "value-one").unwrap();
        store.encrypt("open sesame").unwrap();
        ilar::secrets::forget_master(&store);

        let mut prompts = Vec::new();
        let mut wrong = |prompt: &str| {
            prompts.push(prompt.to_string());
            Ok("nope".to_string())
        };
        assert!(!unlock_if_sealed(&store, STARTUP_PROMPT, STARTUP_TRIES, &mut wrong).unwrap());
        assert_eq!(prompts.len(), STARTUP_TRIES);
        assert_eq!(prompts[0], STARTUP_PROMPT);
        assert!(
            prompts[1].starts_with("Wrong master password. "),
            "{prompts:?}"
        );
        assert!(store.is_locked());

        // Right on the second try: unlocked, and nothing else asks.
        let mut answers = vec!["nope".to_string(), "open sesame".to_string()];
        let mut second = |_: &str| Ok(answers.remove(0));
        assert!(unlock_if_sealed(&store, STARTUP_PROMPT, STARTUP_TRIES, &mut second).unwrap());
        assert!(!store.is_locked());
        assert!(answers.is_empty());

        // No terminal to ask on: the caller sees the failure and
        // decides to run locked; nothing is unlocked behind its back.
        ilar::secrets::forget_master(&store);
        let mut no_tty = |_: &str| anyhow::bail!("reading the master password: no tty");
        let error =
            unlock_if_sealed(&store, STARTUP_PROMPT, STARTUP_TRIES, &mut no_tty).unwrap_err();
        assert!(error.to_string().contains("no tty"), "{error}");
        assert!(store.is_locked());

        // Enter leaves it locked without spending a try.
        let mut enter = |_: &str| Ok(String::new());
        assert!(!unlock_if_sealed(&store, STARTUP_PROMPT, STARTUP_TRIES, &mut enter).unwrap());

        // One try — `ilar secret …` — reports a wrong password as one.
        let mut once = |_: &str| Ok("nope".to_string());
        assert!(unlock_if_sealed(&store, CLI_PROMPT, 1, &mut once).is_err());
    }

    /// A backgrounded job must not be asked: touching the terminal
    /// stops it, leaving a stopped job and no prompt.
    #[cfg(unix)]
    #[test]
    fn a_background_job_is_never_prompted() {
        // Foreground: our own group owns the terminal.
        assert!(super::prompt_can_be_answered(4321, 4321));
        // Background: some other group owns it.
        assert!(!super::prompt_can_be_answered(4321, 8765));
        // The terminal named no owner. A different failure, and one
        // the read reports for itself rather than hanging on.
        assert!(super::prompt_can_be_answered(-1, 8765));
    }
}
