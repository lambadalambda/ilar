//! `ilar secret …`: the store's command line. Values come in on stdin,
//! never as an argument, so they stay out of shell history and process
//! listings.

use anyhow::{Context, Result};
use ilar::secrets::SecretStore;

#[derive(clap::Subcommand, Debug, Clone, PartialEq, Eq)]
pub(crate) enum SecretCommand {
    /// Store a secret; the value is read from stdin (one line, or a pipe)
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
    Remove { name: String },
    /// Let a tool use a secret without asking, for good
    Grant {
        name: String,
        /// The tool: bash or service
        #[arg(long)]
        tool: String,
    },
    /// Drop standing grants, for one tool or all of them
    Revoke {
        name: String,
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
    rpassword::prompt_password(prompt).context("reading the master password")
}

/// Unlock a sealed store for this process, asking once. An empty
/// answer leaves it locked; the caller says what that costs.
pub(crate) fn unlock_if_sealed(store: &SecretStore, ask: AskPassword<'_>) -> Result<bool> {
    if !store.is_locked() {
        return Ok(true);
    }
    let password = ask("Secret store master password (Enter leaves it locked): ")?;
    if password.trim().is_empty() {
        return Ok(false);
    }
    store.unlock(&password)?;
    Ok(true)
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
    match &command {
        SecretCommand::Encrypt => {
            if store.is_sealed() {
                anyhow::bail!(
                    "the store is already sealed; decrypt it first to change the password"
                );
            }
            let password = ask("New master password: ")?;
            if password != ask("Again: ")? {
                anyhow::bail!("the two did not match");
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
            if !unlock_if_sealed(store, ask)? {
                anyhow::bail!(
                    "the store is sealed; nothing can be read or written without the master password"
                );
            }
        }
    }
    match command {
        SecretCommand::Set { name, description } => {
            ilar::secrets::valid_name(&name).map_err(anyhow::Error::msg)?;
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
                    "No secrets stored. Add one with: ilar secret set NAME ({})",
                    store.path().display()
                ));
            }
            Ok(listed
                .iter()
                .map(|secret| {
                    let mut line = secret.name.clone();
                    if !secret.description.is_empty() {
                        line.push_str(&format!("  {}", secret.description));
                    }
                    if !secret.always.is_empty() {
                        line.push_str(&format!("  [always: {}]", secret.always.join(", ")));
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        SecretCommand::Remove { name } => Ok(if store.remove(&name)? {
            format!("Removed {name}")
        } else {
            format!("No secret named {name}")
        }),
        SecretCommand::Grant { name, tool } => {
            if !ilar::secrets::GRANTABLE_TOOLS.contains(&tool.as_str()) {
                anyhow::bail!(
                    "no tool named {tool} takes secrets; one of: {}",
                    ilar::secrets::GRANTABLE_TOOLS.join(", ")
                );
            }
            Ok(if store.grant_always(&name, &tool)? {
                format!("{tool} may use {name} without asking")
            } else {
                format!("No secret named {name}")
            })
        }
        SecretCommand::Revoke { name, tool } => Ok(if store.revoke(&name, tool.as_deref())? {
            match tool {
                Some(tool) => format!("{tool} will ask for {name} again"),
                None => format!("Every tool will ask for {name} again"),
            }
        } else {
            format!("Nothing to revoke for {name}")
        }),
        SecretCommand::Encrypt | SecretCommand::Decrypt => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_password(_: &str) -> Result<String> {
        panic!("a plain store asks for no master password")
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
        assert_eq!(out, "GITHUB_TOKEN  for gh  [always: bash]");
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
        let mut answers = vec!["open sesame".to_string(), "open sesame".to_string()];
        let mut ask = |_: &str| Ok(answers.remove(0));
        let out = run(&store, SecretCommand::Encrypt, Some(&mut none), &mut ask).unwrap();
        assert!(out.starts_with("Sealed "), "{out}");
        assert!(store.is_sealed());
        // Forget it, as a new process would.
        ilar::secrets::forget_master(&store);
        assert!(store.is_locked());
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
}
