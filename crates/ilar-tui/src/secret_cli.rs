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
}

/// Run one command against the store; the text is what to print.
pub(crate) fn run(
    store: &SecretStore,
    command: SecretCommand,
    stdin: &mut dyn std::io::Read,
) -> Result<String> {
    match command {
        SecretCommand::Set { name, description } => {
            ilar::secrets::valid_name(&name).map_err(anyhow::Error::msg)?;
            let mut value = String::new();
            stdin
                .read_to_string(&mut value)
                .context("reading the value from stdin")?;
            let value = value.trim_end_matches(['\n', '\r']);
            if value.is_empty() {
                anyhow::bail!("no value on stdin; pipe it in, or type it and end with Ctrl-D");
            }
            let replaced = store.set(&name, &description, value)?;
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            &mut stdin,
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
                &mut empty
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
            &mut none,
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
                &mut none
            )
            .is_err()
        );
        let out = run(&store, SecretCommand::List, &mut none).unwrap();
        assert_eq!(out, "GITHUB_TOKEN  for gh  [always: bash]");
        assert!(!out.contains("ghp_secret"));
        let out = run(
            &store,
            SecretCommand::Revoke {
                name: "GITHUB_TOKEN".into(),
                tool: None,
            },
            &mut none,
        )
        .unwrap();
        assert_eq!(out, "Every tool will ask for GITHUB_TOKEN again");
        let out = run(
            &store,
            SecretCommand::Remove {
                name: "GITHUB_TOKEN".into(),
            },
            &mut none,
        )
        .unwrap();
        assert_eq!(out, "Removed GITHUB_TOKEN");
        assert!(
            run(&store, SecretCommand::List, &mut none)
                .unwrap()
                .starts_with("No secrets stored")
        );
    }
}
