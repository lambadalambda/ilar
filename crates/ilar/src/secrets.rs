//! The secrets a person hands ilar: API keys, tokens, passwords. Kept
//! by name in one file, shown to the model as names and descriptions
//! only, and handed to a command as an environment variable once the
//! person has said yes to that use.
//!
//! Three parts. [`SecretStore`] is the file. [`Secrets`] is what a tool
//! holds: it resolves names to values, asking through a
//! [`GrantSender`] when a use is not yet granted. The grant protocol is
//! shaped like the question protocol: a prompt over a channel with a
//! one-shot reply, so any driver with somebody to ask can answer it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Under the state directory.
pub const FILE_NAME: &str = "secrets.json";
pub const MAX_NAME_CHARS: usize = 64;
/// The tools that take `secrets`, by name: what a standing grant may name.
pub const GRANTABLE_TOOLS: &[&str] = &["bash", "service"];
/// A value shorter than this is refused at the door: replacing "ab"
/// wherever it appears in output, or hiding every variable that
/// happens to equal "1", mangles more than it protects.
pub const MIN_VALUE_CHARS: usize = 4;

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    secrets: BTreeMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    #[serde(default)]
    description: String,
    value: String,
    /// Tools this secret is granted to for good.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    always: BTreeSet<String>,
}

/// One entry as a listing shows it: everything but the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub description: String,
    pub always: Vec<String>,
}

/// A name is an environment variable name: letters, digits and
/// underscores, not starting with a digit.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a secret needs a name".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "a secret name is at most {MAX_NAME_CHARS} characters"
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "{name:?} is not a secret name: use letters, digits and underscores, \
             not starting with a digit, like GITHUB_TOKEN"
        ));
    }
    Ok(())
}

/// The file: `<state dir>/secrets.json`, mode 0600, rewritten whole
/// under a lock so the CLI and a running agent do not cross.
#[derive(Debug, Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    pub fn open(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Result<File> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(File::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading secrets {}", self.path.display()));
            }
        };
        serde_json::from_str(&content)
            .with_context(|| format!("parsing secrets {}", self.path.display()))
    }

    fn save(&self, file: &File) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic_file::replace(
            &self.path,
            serde_json::to_string_pretty(file)?.as_bytes(),
            crate::atomic_file::Mode::Force(0o600),
        )
        .with_context(|| format!("writing secrets {}", self.path.display()))?;
        Ok(())
    }

    /// Read, change, write, under the lock.
    fn update<T>(&self, change: impl FnOnce(&mut File) -> T) -> Result<T> {
        let _lock = self.lock()?;
        let mut file = self.load()?;
        let outcome = change(&mut file);
        self.save(&file)?;
        Ok(outcome)
    }

    fn lock(&self) -> Result<std::fs::File> {
        let path = self.path.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        FileExt::lock_exclusive(&file).with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    /// Every secret, without its value, by name.
    pub fn list(&self) -> Result<Vec<Listed>> {
        Ok(self
            .load()?
            .secrets
            .into_iter()
            .map(|(name, entry)| Listed {
                name,
                description: entry.description,
                always: entry.always.into_iter().collect(),
            })
            .collect())
    }

    /// Store a secret. A name already there keeps its grants: a rotated
    /// key is the same secret. Returns whether it replaced one.
    pub fn set(&self, name: &str, description: &str, value: &str) -> Result<bool> {
        valid_name(name).map_err(anyhow::Error::msg)?;
        if value.chars().count() < MIN_VALUE_CHARS {
            anyhow::bail!(
                "the value of {name} is under {MIN_VALUE_CHARS} characters; that is not a secret"
            );
        }
        self.update(|file| {
            let previous = file.secrets.remove(name);
            let replaced = previous.is_some();
            let always = previous.map(|entry| entry.always).unwrap_or_default();
            file.secrets.insert(
                name.to_string(),
                Entry {
                    description: description.trim().to_string(),
                    value: value.to_string(),
                    always,
                },
            );
            replaced
        })
    }

    /// Forget a secret and its grants. Returns whether there was one.
    pub fn remove(&self, name: &str) -> Result<bool> {
        self.update(|file| file.secrets.remove(name).is_some())
    }

    /// Grant `tool` the secret for good. Returns whether the secret exists.
    pub fn grant_always(&self, name: &str, tool: &str) -> Result<bool> {
        self.update(|file| match file.secrets.get_mut(name) {
            Some(entry) => {
                entry.always.insert(tool.to_string());
                true
            }
            None => false,
        })
    }

    /// Drop the standing grants of a secret, for one tool or all.
    /// Returns whether any was dropped.
    pub fn revoke(&self, name: &str, tool: Option<&str>) -> Result<bool> {
        self.update(|file| match file.secrets.get_mut(name) {
            Some(entry) => match tool {
                Some(tool) => entry.always.remove(tool),
                None => !std::mem::take(&mut entry.always).is_empty(),
            },
            None => false,
        })
    }

    /// The value, for ilar's own use: a provider key the config reads.
    /// Nothing that talks to the model calls this.
    pub fn value(&self, name: &str) -> Result<Option<String>> {
        Ok(self.load()?.secrets.remove(name).map(|entry| entry.value))
    }

    /// Whether there is nothing to list. A store that cannot be read
    /// is empty here; the resolve path is where that is reported.
    pub fn is_empty(&self) -> bool {
        self.list().map(|listed| listed.is_empty()).unwrap_or(true)
    }

    /// Every entry, name and value, for shielding a child environment
    /// and redacting output.
    fn all(&self) -> Result<Vec<Granted>> {
        Ok(self
            .load()?
            .secrets
            .into_iter()
            .map(|(name, entry)| Granted {
                name,
                value: entry.value,
            })
            .collect())
    }
}

/// How long a use is granted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    /// This one call.
    Once,
    /// This tool, until the runtime ends.
    Session,
    /// This tool, for good: written to the store.
    Always,
}

/// A request to use a secret, delivered over [`GrantSender`] with its
/// one-shot reply path. `None` in the reply is a refusal.
#[derive(Debug)]
pub struct GrantPrompt {
    pub session_id: String,
    pub tool_call_id: Option<String>,
    /// The tool asking, `bash` or `service`.
    pub tool: String,
    pub secret: String,
    pub description: String,
    /// What the tool is about to run with it: the command, verbatim.
    /// The person reads this before saying yes; it is the whole safety.
    pub detail: String,
    pub reply: oneshot::Sender<Option<Grant>>,
}

pub type GrantSender = mpsc::Sender<GrantPrompt>;
pub type GrantReceiver = mpsc::Receiver<GrantPrompt>;

pub fn grant_channel(capacity: usize) -> (GrantSender, GrantReceiver) {
    mpsc::channel(capacity)
}

/// A secret a tool may use now: the name it goes under and the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Granted {
    pub name: String,
    value: String,
}

impl Granted {
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for Granted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Granted({})", self.name)
    }
}

/// One tool call's ask.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub tool: &'a str,
    pub names: &'a [String],
    /// Shown to the person: the command, verbatim.
    pub detail: &'a str,
    pub session_id: &'a str,
    pub tool_call_id: Option<&'a str>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
}

/// What a tool holds: the store, the grants given for this runtime,
/// and somebody to ask, when there is one.
#[derive(Clone)]
pub struct Secrets {
    store: SecretStore,
    /// `(secret, tool)` pairs granted for the session.
    session: Arc<Mutex<BTreeSet<(String, String)>>>,
    prompts: Option<GrantSender>,
}

/// The CLI line a call without a grant and without anyone to ask
/// points at.
fn grant_hint(name: &str, tool: &str) -> String {
    format!("the user can allow it with: ilar secret grant {name} --tool {tool}")
}

impl Secrets {
    pub fn new(store: SecretStore) -> Self {
        Self {
            store,
            session: Arc::new(Mutex::new(BTreeSet::new())),
            prompts: None,
        }
    }

    pub fn with_prompts(mut self, sender: GrantSender) -> Self {
        self.prompts = Some(sender);
        self
    }

    pub fn store(&self) -> &SecretStore {
        &self.store
    }

    pub fn can_ask(&self) -> bool {
        self.prompts.is_some()
    }

    /// The listing the model gets: names, descriptions, standing grants.
    pub fn listing(&self) -> Result<String> {
        let listed = self.store.list()?;
        if listed.is_empty() {
            return Ok(
                "No secrets are stored. The user adds one with: ilar secret set NAME".to_string(),
            );
        }
        let session = self.session.lock().unwrap();
        let lines = listed
            .iter()
            .map(|secret| {
                let mut line = secret.name.clone();
                if !secret.description.is_empty() {
                    line.push_str(" — ");
                    line.push_str(&secret.description);
                }
                let mut tools: Vec<String> = secret
                    .always
                    .iter()
                    .map(|tool| format!("{tool} (always)"))
                    .collect();
                tools.extend(
                    session
                        .iter()
                        .filter(|(name, _)| *name == secret.name)
                        .map(|(_, tool)| format!("{tool} (this session)")),
                );
                if !tools.is_empty() {
                    line.push_str(&format!(" [granted: {}]", tools.join(", ")));
                }
                line
            })
            .collect::<Vec<_>>();
        Ok(lines.join("\n"))
    }

    /// Every stored secret, name and value, for shielding a child
    /// environment and redacting output. An unreadable store yields
    /// nothing here; the resolve path is where that gets reported.
    pub fn all(&self) -> Vec<Granted> {
        self.store.all().unwrap_or_default()
    }

    /// Text with every stored value replaced, whether or not this call
    /// was granted it: what a tool result may carry.
    pub fn scrub(&self, text: &str) -> String {
        redact(text, &self.all())
    }

    /// The values the call may use, each granted by a standing grant,
    /// a session grant, or the person just now. The first refusal or
    /// failure ends the whole call: a command that expected three
    /// variables and got two is not the command the person read.
    pub async fn resolve(&self, request: Request<'_>) -> Result<Vec<Granted>, String> {
        // Every name looked up before anyone is asked: a call that
        // names one unknown secret fails without a question.
        let file = self
            .store
            .load()
            .map_err(|error| format!("secrets: {error:#}"))?;
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        for name in request.names {
            if !seen.insert(name.as_str()) {
                continue;
            }
            match file.secrets.get(name) {
                Some(entry) => entries.push((name, entry)),
                None => return Err(self.unknown(name)),
            }
        }
        let mut granted = Vec::new();
        for (name, entry) in entries {
            if !entry.always.contains(request.tool) && !self.session_granted(name, request.tool) {
                self.ask(&request, name, &entry.description).await?;
            }
            granted.push(Granted {
                name: name.clone(),
                value: entry.value.clone(),
            });
        }
        Ok(granted)
    }

    fn session_granted(&self, name: &str, tool: &str) -> bool {
        self.session
            .lock()
            .unwrap()
            .contains(&(name.to_string(), tool.to_string()))
    }

    fn unknown(&self, name: &str) -> String {
        let known: Vec<String> = self
            .store
            .list()
            .map(|listed| listed.into_iter().map(|secret| secret.name).collect())
            .unwrap_or_default();
        if known.is_empty() {
            format!(
                "no secret named {name}: none are stored; the user adds one with: ilar secret set {name}"
            )
        } else {
            format!(
                "no secret named {name}; stored: {}. The user adds one with: ilar secret set {name}",
                known.join(", ")
            )
        }
    }

    async fn ask(
        &self,
        request: &Request<'_>,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        let Some(sender) = &self.prompts else {
            return Err(format!(
                "{name} is not granted to {}, and nobody is here to ask; {}",
                request.tool,
                grant_hint(name, request.tool)
            ));
        };
        let (reply, receive) = oneshot::channel();
        let prompt = GrantPrompt {
            session_id: request.session_id.to_string(),
            tool_call_id: request.tool_call_id.map(str::to_string),
            tool: request.tool.to_string(),
            secret: name.to_string(),
            description: description.to_string(),
            detail: request.detail.to_string(),
            reply,
        };
        let unavailable = || {
            format!(
                "{name} is not granted to {}, and nobody answered; {}",
                request.tool,
                grant_hint(name, request.tool)
            )
        };
        let delivered = tokio::select! {
            delivered = sender.send(prompt) => delivered,
            _ = request.cancel.cancelled() => return Err("cancelled while asking for a secret".into()),
        };
        if delivered.is_err() {
            return Err(unavailable());
        }
        let answer = tokio::select! {
            answer = receive => answer,
            _ = request.cancel.cancelled() => return Err("cancelled while asking for a secret".into()),
        };
        match answer {
            Ok(Some(Grant::Once)) => Ok(()),
            Ok(Some(Grant::Session)) => {
                self.session
                    .lock()
                    .unwrap()
                    .insert((name.to_string(), request.tool.to_string()));
                Ok(())
            }
            Ok(Some(Grant::Always)) => {
                // The person said yes; a store that cannot be written
                // makes that a session grant rather than a refusal.
                if self.store.grant_always(name, request.tool).is_err() {
                    self.session
                        .lock()
                        .unwrap()
                        .insert((name.to_string(), request.tool.to_string()));
                }
                Ok(())
            }
            Ok(None) => Err(format!(
                "the user denied {name} for this {} call",
                request.tool
            )),
            Err(_) => Err(unavailable()),
        }
    }
}

/// Output with every granted value replaced by `<secret:NAME>`. At the
/// source, before a spill file or a transcript sees it: this is the
/// one redaction in ilar that is not display-only.
pub fn redact(text: &str, granted: &[Granted]) -> String {
    // Replacing whole UTF-8 values with ASCII marks keeps UTF-8 valid.
    String::from_utf8(redact_bytes(text.as_bytes(), granted)).expect("redaction keeps UTF-8")
}

/// [`redact`] for captured bytes, which may not be UTF-8.
pub fn redact_bytes(bytes: &[u8], granted: &[Granted]) -> Vec<u8> {
    let mut bytes = std::borrow::Cow::Borrowed(bytes);
    for secret in longest_first(granted) {
        let needle = secret.value().as_bytes();
        if bytes.windows(needle.len()).any(|window| window == needle) {
            let mark = format!("<secret:{}>", secret.name);
            let mut out = Vec::with_capacity(bytes.len());
            let mut rest: &[u8] = &bytes;
            while let Some(at) = rest
                .windows(needle.len())
                .position(|window| window == needle)
            {
                out.extend_from_slice(&rest[..at]);
                out.extend_from_slice(mark.as_bytes());
                rest = &rest[at + needle.len()..];
            }
            out.extend_from_slice(rest);
            bytes = std::borrow::Cow::Owned(out);
        }
    }
    bytes.into_owned()
}

/// Longest value first, so one that contains another is replaced whole
/// and none shorter than [`MIN_VALUE_CHARS`] (a file from before the
/// floor may hold one).
fn longest_first(granted: &[Granted]) -> Vec<&Granted> {
    let mut secrets: Vec<&Granted> = granted
        .iter()
        .filter(|secret| secret.value().chars().count() >= MIN_VALUE_CHARS)
        .collect();
    secrets.sort_by(|a, b| b.value().len().cmp(&a.value().len()));
    secrets
}

/// Which of the process's own variables a child must not see: ilar's
/// keys by name, and anything whose value is a stored secret.
pub fn shielded_env(stored: &[Granted]) -> Vec<String> {
    // `vars_os`, not `vars`: one non-UTF-8 variable in the environment
    // must not panic every shell command. A lossy name cannot match
    // anything to remove, which is the right outcome for it.
    shielded_env_from(
        std::env::vars_os().map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        }),
        stored,
    )
}

pub fn shielded_env_from(
    vars: impl IntoIterator<Item = (String, String)>,
    stored: &[Granted],
) -> Vec<String> {
    let values = longest_first(stored);
    vars.into_iter()
        .filter(|(name, value)| {
            ilar_own_secret(name) || values.iter().any(|secret| secret.value() == value)
        })
        .map(|(name, _)| name)
        .collect()
}

fn ilar_own_secret(name: &str) -> bool {
    (name.starts_with("ILAR_") && name.ends_with("_API_KEY")) || name == "ILAR_SERVE_TOKEN"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::open(&dir.path().join("state"));
        (dir, store)
    }

    fn cancel() -> tokio_util::sync::CancellationToken {
        tokio_util::sync::CancellationToken::new()
    }

    fn request<'a>(
        names: &'a [String],
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> Request<'a> {
        Request {
            tool: "bash",
            names,
            detail: "gh pr list",
            session_id: "s1",
            tool_call_id: Some("c1"),
            cancel,
        }
    }

    #[test]
    fn the_store_keeps_names_and_shows_no_values() {
        let (_dir, store) = store();
        assert!(store.list().unwrap().is_empty());
        assert!(!store.set("GITHUB_TOKEN", " for gh ", "ghp_abc").unwrap());
        assert!(store.set("GITHUB_TOKEN", "for gh", "ghp_def").unwrap());
        let listed = store.list().unwrap();
        assert_eq!(
            listed,
            [Listed {
                name: "GITHUB_TOKEN".into(),
                description: "for gh".into(),
                always: vec![]
            }]
        );
        assert_eq!(
            store.value("GITHUB_TOKEN").unwrap().as_deref(),
            Some("ghp_def")
        );
        assert_eq!(store.value("OTHER").unwrap(), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(store.remove("GITHUB_TOKEN").unwrap());
        assert!(!store.remove("GITHUB_TOKEN").unwrap());
    }

    #[test]
    fn names_are_environment_names_and_values_are_not_empty() {
        let (_dir, store) = store();
        assert!(store.set("1BAD", "", "xxxx").is_err());
        assert!(store.set("BAD-NAME", "", "xxxx").is_err());
        assert!(store.set("", "", "xxxx").is_err());
        assert!(store.set("EMPTY", "", "").is_err());
        assert!(store.set("SHORT", "", "abc").is_err());
        assert!(valid_name("_ok_1").is_ok());
        assert!(valid_name(&"A".repeat(MAX_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn standing_grants_survive_a_rotation_and_a_revoke_drops_them() {
        let (_dir, store) = store();
        assert!(!store.grant_always("NOPE", "bash").unwrap());
        store.set("KEY", "", "one-value").unwrap();
        assert!(store.grant_always("KEY", "bash").unwrap());
        assert!(store.grant_always("KEY", "service").unwrap());
        store.set("KEY", "", "two-value").unwrap();
        assert_eq!(store.list().unwrap()[0].always, ["bash", "service"]);
        assert!(store.revoke("KEY", Some("bash")).unwrap());
        assert_eq!(store.list().unwrap()[0].always, ["service"]);
        assert!(store.revoke("KEY", None).unwrap());
        assert!(!store.revoke("KEY", None).unwrap());
        assert!(store.list().unwrap()[0].always.is_empty());
    }

    /// One unknown name fails the call before anyone is asked about
    /// the known ones.
    #[tokio::test]
    async fn an_unknown_name_fails_before_any_question() {
        let (_dir, store) = store();
        store.set("KEY", "", "value1").unwrap();
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store).with_prompts(tx);
        let cancel = cancel();
        let names = ["KEY".to_string(), "NOPE".to_string()];
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("no secret named NOPE"), "{error}");
        assert!(rx.try_recv().is_err(), "somebody was asked about KEY");
    }

    #[tokio::test]
    async fn an_unknown_name_lists_the_known_ones() {
        let (_dir, store) = store();
        let secrets = Secrets::new(store.clone());
        let cancel = cancel();
        let names = ["NOPE".to_string()];
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("none are stored"), "{error}");
        store.set("KEY", "", "value1").unwrap();
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("stored: KEY"), "{error}");
        assert!(!error.contains("value1"), "{error}");
    }

    #[tokio::test]
    async fn nobody_to_ask_means_no_and_names_the_cli() {
        let (_dir, store) = store();
        store.set("KEY", "", "value1").unwrap();
        let secrets = Secrets::new(store.clone());
        let cancel = cancel();
        let names = ["KEY".to_string()];
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(
            error.contains("ilar secret grant KEY --tool bash"),
            "{error}"
        );
        store.grant_always("KEY", "bash").unwrap();
        let granted = secrets.resolve(request(&names, &cancel)).await.unwrap();
        assert_eq!(granted[0].name, "KEY");
        assert_eq!(granted[0].value(), "value1");
        assert_eq!(format!("{granted:?}"), "[Granted(KEY)]");
    }

    /// Answer the next prompt, returning what it asked.
    async fn answer(rx: &mut GrantReceiver, grant: Option<Grant>) -> GrantPrompt {
        let prompt = rx.recv().await.unwrap();
        let (tool, secret, detail) = (
            prompt.tool.clone(),
            prompt.secret.clone(),
            prompt.detail.clone(),
        );
        let GrantPrompt {
            reply,
            session_id,
            tool_call_id,
            description,
            ..
        } = prompt;
        reply.send(grant).unwrap();
        let (dead, _) = oneshot::channel();
        GrantPrompt {
            session_id,
            tool_call_id,
            tool,
            secret,
            description,
            detail,
            reply: dead,
        }
    }

    #[tokio::test]
    async fn each_answer_grants_as_far_as_it_says() {
        let (_dir, store) = store();
        store.set("KEY", "the key", "value1").unwrap();
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store.clone()).with_prompts(tx);
        let cancel = cancel();
        let names = ["KEY".to_string(), "KEY".to_string()];

        // Denied.
        let asking = secrets.resolve(request(&names, &cancel));
        let (outcome, asked) = tokio::join!(asking, answer(&mut rx, None));
        assert!(outcome.unwrap_err().contains("denied KEY"));
        assert_eq!(
            (asked.tool.as_str(), asked.secret.as_str()),
            ("bash", "KEY")
        );
        assert_eq!(asked.detail, "gh pr list");
        assert_eq!(asked.description, "the key");
        assert_eq!(asked.tool_call_id.as_deref(), Some("c1"));

        // Once: asked again next time.
        let (outcome, _) = tokio::join!(
            secrets.resolve(request(&names, &cancel)),
            answer(&mut rx, Some(Grant::Once))
        );
        assert_eq!(
            outcome.unwrap().len(),
            1,
            "a name given twice is one secret"
        );
        let (outcome, _) = tokio::join!(
            secrets.resolve(request(&names, &cancel)),
            answer(&mut rx, Some(Grant::Session))
        );
        assert!(outcome.is_ok());
        // Session: no prompt now; another tool still asks.
        assert!(secrets.resolve(request(&names, &cancel)).await.is_ok());
        assert!(secrets.listing().unwrap().contains("bash (this session)"));
        let service = Request {
            tool: "service",
            ..request(&names, &cancel)
        };
        let (outcome, _) = tokio::join!(
            secrets.resolve(service),
            answer(&mut rx, Some(Grant::Always))
        );
        assert!(outcome.is_ok());
        assert_eq!(store.list().unwrap()[0].always, ["service"]);
        // A fresh runtime keeps the standing grant and not the session one.
        let fresh = Secrets::new(store.clone());
        assert!(fresh.resolve(service).await.is_ok());
        assert!(fresh.resolve(request(&names, &cancel)).await.is_err());
    }

    #[tokio::test]
    async fn a_dropped_frontend_is_a_no_and_a_cancel_stops_the_wait() {
        let (_dir, store) = store();
        store.set("KEY", "", "value1").unwrap();
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store.clone()).with_prompts(tx);
        let cancel = cancel();
        let names = ["KEY".to_string()];
        let (outcome, _) = tokio::join!(secrets.resolve(request(&names, &cancel)), async {
            drop(rx.recv().await.unwrap());
        });
        assert!(outcome.unwrap_err().contains("nobody answered"));

        // The prompt outlives the cancel, so the only thing that can
        // end the wait is the cancel itself.
        let (outcome, prompt) = tokio::join!(secrets.resolve(request(&names, &cancel)), async {
            let prompt = rx.recv().await.unwrap();
            cancel.cancel();
            prompt
        });
        assert!(outcome.unwrap_err().contains("cancelled"));
        drop(prompt);
    }

    #[test]
    fn output_loses_the_values_it_echoes() {
        let granted = vec![
            Granted {
                name: "A".into(),
                value: "abcd".into(),
            },
            Granted {
                name: "B".into(),
                value: "abcdef".into(),
            },
            Granted {
                name: "C".into(),
                value: "xy".into(),
            },
        ];
        assert_eq!(
            redact("token abcdef and abcd and xy", &granted),
            "token <secret:B> and <secret:A> and xy"
        );
        assert_eq!(
            redact_bytes(b"\xffabcdef\xff", &granted),
            b"\xff<secret:B>\xff".to_vec()
        );
        assert_eq!(redact("nothing", &granted), "nothing");
    }

    #[test]
    fn a_child_is_shielded_from_ilars_keys_and_stored_values() {
        let vars = [
            ("ILAR_OPENAI_API_KEY", "sk-1"),
            ("ILAR_SERVE_TOKEN", "t"),
            ("ILAR_STATE_DIR", "/tmp"),
            ("MY_TOKEN", "stored-value"),
            ("PATH", "/bin"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let stored = vec![
            Granted {
                name: "MINE".into(),
                value: "stored-value".into(),
            },
            Granted {
                name: "TINY".into(),
                value: "/bin".into(),
            },
        ];
        let mut hidden = shielded_env_from(vars, &stored);
        hidden.sort();
        assert_eq!(
            hidden,
            [
                "ILAR_OPENAI_API_KEY",
                "ILAR_SERVE_TOKEN",
                "MY_TOKEN",
                "PATH"
            ]
        );
        // A stored value under the floor (an old file) hides nothing.
        let short = vec![Granted {
            name: "S".into(),
            value: "/bi".into(),
        }];
        assert!(shielded_env_from([("PATH".to_string(), "/bi".to_string())], &short).is_empty());
    }
}
