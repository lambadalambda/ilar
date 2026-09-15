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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

/// Under the state directory.
pub const FILE_NAME: &str = "secrets.json";
pub const MAX_NAME_CHARS: usize = 64;
/// The tools that take `secrets`, by name: what a standing grant may name.
pub const GRANTABLE_TOOLS: &[&str] = &["bash", "service", "sudo"];
/// The pseudo-secret the sudo tool asks for: root has no value to hand
/// over, only a yes. Reserved: it cannot be stored.
pub const ROOT: &str = "root";
/// What the `root` row says it is for.
pub const ROOT_DESCRIPTION: &str = "superuser, for the sudo tool";
/// The secret that answers sudo's password prompt: typed into the
/// grant prompt and held for the session, or stored under this name.
pub const SUDO_PASSWORD: &str = "SUDO_PASSWORD";
/// A value shorter than this is refused at the door: replacing "ab"
/// wherever it appears in output, or hiding every variable that
/// happens to equal "1", mangles more than it protects.
pub const MIN_VALUE_CHARS: usize = 4;

/// The file sealed under a master password: the key is derived from
/// the password and `salt` with Argon2id, the JSON is XChaCha20-Poly1305
/// under a fresh nonce every write.
#[derive(Serialize, Deserialize)]
struct Sealed {
    version: u32,
    kdf: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

/// What is on disk: the JSON as it is, or sealed.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Disk {
    Sealed { sealed: Sealed },
    Plain(File),
}

/// The store is sealed and this process has not been given the master
/// password. Callers tell it apart from a broken file, since the cure
/// is different. How to unlock it depends on the driver, which is why
/// the way out is not in this text: see [`Secrets::with_unlock_hint`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the secret store is sealed and locked")]
pub struct Locked;

/// The file was sealed again while this process held its password: a
/// second process changed the master password. The held password is
/// dropped, so the store reads as locked again and a driver with
/// somebody to ask asks for the new one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the secret store was sealed again since this process opened it, and the master password it holds does not open it"
)]
pub struct Resealed;

/// The unlocked key for one sealed store, held for the life of the
/// process. The password stays too: a store re-sealed under another
/// salt by a second process is re-derived rather than refused.
struct Master {
    password: Zeroizing<String>,
    salt: Vec<u8>,
    key: Zeroizing<[u8; 32]>,
}

static MASTERS: LazyLock<Mutex<HashMap<PathBuf, Master>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Drop the held master for a store: what a new process starts as.
pub fn forget_master(store: &SecretStore) {
    MASTERS.lock().unwrap().remove(store.path());
}

const SEAL_VERSION: u32 = 1;
const KDF: &str = "argon2id";

/// Argon2id at its defaults (19 MiB, two passes). The unit tests run
/// it far lighter: a debug build derives at these settings in seconds,
/// and their stores are throwaways.
fn kdf() -> argon2::Argon2<'static> {
    #[cfg(test)]
    {
        argon2::Argon2::new(
            argon2::Algorithm::Argon2id,
            argon2::Version::V0x13,
            argon2::Params::new(1024, 1, 1, Some(32)).expect("valid params"),
        )
    }
    #[cfg(not(test))]
    {
        argon2::Argon2::default()
    }
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0u8; 32]);
    kdf()
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|error| anyhow::anyhow!("deriving the master key: {error}"))?;
    Ok(key)
}

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str, what: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .with_context(|| format!("the sealed store's {what} is not base64"))
}

fn seal(file: &File, master: &Master) -> Result<Sealed> {
    let plaintext = Zeroizing::new(serde_json::to_vec(file)?);
    let cipher = XChaCha20Poly1305::new(master.key.as_ref().into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_slice())
        .map_err(|_| anyhow::anyhow!("sealing the secret store"))?;
    Ok(Sealed {
        version: SEAL_VERSION,
        kdf: KDF.into(),
        salt: base64(&master.salt),
        nonce: base64(&nonce),
        ciphertext: base64(&ciphertext),
    })
}

fn unseal(sealed: &Sealed, key: &[u8; 32]) -> Result<File> {
    if sealed.version != SEAL_VERSION || sealed.kdf != KDF {
        anyhow::bail!(
            "the sealed store is version {} with {}; this ilar reads version {SEAL_VERSION} with {KDF}",
            sealed.version,
            sealed.kdf
        );
    }
    let nonce = unbase64(&sealed.nonce, "nonce")?;
    if nonce.len() != 24 {
        anyhow::bail!("the sealed store's nonce is {} bytes, not 24", nonce.len());
    }
    let ciphertext = unbase64(&sealed.ciphertext, "ciphertext")?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(XNonce::from_slice(&nonce), ciphertext.as_slice())
            .map_err(|_| anyhow::anyhow!("wrong master password"))?,
    );
    serde_json::from_slice(&plaintext).context("parsing the unsealed store")
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    secrets: BTreeMap<String, Entry>,
    /// Tools with standing approval to run as root: `sudo`, when the
    /// person answered "always".
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    root: BTreeSet<String>,
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

    /// The file as it is on disk; a missing file is an empty plain one.
    fn read_disk(&self) -> Result<Disk> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Disk::Plain(File::default()));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading secrets {}", self.path.display()));
            }
        };
        // Strict: a file that names a seal is read as one or refused.
        // Read loosely, a sealed file with one field wrong would look
        // like an empty plain store, and the next write would replace
        // it with exactly that.
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing secrets {}", self.path.display()))?;
        if let Some(sealed) = value.get("sealed") {
            let sealed: Sealed = serde_json::from_value(sealed.clone())
                .with_context(|| format!("the seal of {} is malformed", self.path.display()))?;
            return Ok(Disk::Sealed { sealed });
        }
        serde_json::from_value(value)
            .map(Disk::Plain)
            .with_context(|| format!("parsing secrets {}", self.path.display()))
    }

    /// The key this process holds for the store, and whether it had to
    /// be re-derived because the file carries another salt than the one
    /// it was unlocked under — a second process resealed it.
    fn master_for(&self, salt: &[u8]) -> Result<Option<(Zeroizing<[u8; 32]>, bool)>> {
        let mut masters = MASTERS.lock().unwrap();
        let Some(master) = masters.get_mut(&self.path) else {
            return Ok(None);
        };
        let rederived = master.salt != salt;
        if rederived {
            master.key = derive_key(&master.password, salt)?;
            master.salt = salt.to_vec();
        }
        Ok(Some((master.key.clone(), rederived)))
    }

    /// The file, unsealed when it is sealed and this process may.
    fn open_disk(&self, disk: Disk) -> Result<File> {
        match disk {
            Disk::Plain(file) => Ok(file),
            Disk::Sealed { sealed } => {
                let salt = unbase64(&sealed.salt, "salt")?;
                match self.master_for(&salt)? {
                    Some((key, rederived)) => unseal(&sealed, &key).map_err(|error| {
                        // Re-sealed under another password: the one this
                        // process holds is worthless, so drop it. The
                        // store then reads as locked and whoever can
                        // ask, asks again.
                        if rederived {
                            MASTERS.lock().unwrap().remove(&self.path);
                            Resealed.into()
                        } else {
                            error
                        }
                    }),
                    None => Err(Locked.into()),
                }
            }
        }
    }

    fn load(&self) -> Result<File> {
        self.open_disk(self.read_disk()?)
    }

    fn write_disk(&self, disk: &Disk) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic_file::replace(
            &self.path,
            serde_json::to_string_pretty(disk)?.as_bytes(),
            crate::atomic_file::Mode::Force(0o600),
        )
        .with_context(|| format!("writing secrets {}", self.path.display()))?;
        Ok(())
    }

    /// Write the file back the way it was found: sealed under the
    /// held master, or plain.
    fn save(&self, file: &File, sealed: bool) -> Result<()> {
        let disk = if sealed {
            let masters = MASTERS.lock().unwrap();
            let master = masters.get(&self.path).ok_or(Locked)?;
            Disk::Sealed {
                sealed: seal(file, master)?,
            }
        } else {
            Disk::Plain(File {
                secrets: file.secrets.clone(),
                root: file.root.clone(),
            })
        };
        self.write_disk(&disk)
    }

    /// Read, change, write, under the lock.
    fn update<T>(&self, change: impl FnOnce(&mut File) -> T) -> Result<T> {
        let _lock = self.lock()?;
        let disk = self.read_disk()?;
        let sealed = matches!(disk, Disk::Sealed { .. });
        let mut file = self.open_disk(disk)?;
        let outcome = change(&mut file);
        self.save(&file, sealed)?;
        Ok(outcome)
    }

    /// Whether the file on disk is sealed under a master password.
    pub fn is_sealed(&self) -> bool {
        matches!(self.read_disk(), Ok(Disk::Sealed { .. }))
    }

    /// Sealed, and this process has not been given the password.
    pub fn is_locked(&self) -> bool {
        self.is_sealed() && !MASTERS.lock().unwrap().contains_key(&self.path)
    }

    /// Give this process the master password: verified against the
    /// file, then held until the process ends. A plain store needs
    /// none and says so.
    pub fn unlock(&self, password: &str) -> Result<()> {
        let Disk::Sealed { sealed } = self.read_disk()? else {
            anyhow::bail!("the secret store is not sealed; nothing to unlock");
        };
        let salt = unbase64(&sealed.salt, "salt")?;
        let key = derive_key(password, &salt)?;
        unseal(&sealed, &key)?;
        MASTERS.lock().unwrap().insert(
            self.path.clone(),
            Master {
                password: Zeroizing::new(password.to_string()),
                salt,
                key,
            },
        );
        Ok(())
    }

    /// Seal a plain store under `password`, and hold it unlocked here.
    pub fn encrypt(&self, password: &str) -> Result<()> {
        if password.chars().count() < MIN_VALUE_CHARS {
            anyhow::bail!("a master password is at least {MIN_VALUE_CHARS} characters");
        }
        let _lock = self.lock()?;
        let Disk::Plain(file) = self.read_disk()? else {
            anyhow::bail!(
                "the secret store is already sealed; decrypt it first to change the password"
            );
        };
        let mut salt = vec![0u8; 16];
        chacha20poly1305::aead::rand_core::RngCore::fill_bytes(&mut OsRng, &mut salt);
        let master = Master {
            password: Zeroizing::new(password.to_string()),
            key: derive_key(password, &salt)?,
            salt,
        };
        self.write_disk(&Disk::Sealed {
            sealed: seal(&file, &master)?,
        })?;
        MASTERS.lock().unwrap().insert(self.path.clone(), master);
        Ok(())
    }

    /// Write a sealed store back as plain, given its password, and
    /// forget the master.
    pub fn decrypt(&self, password: &str) -> Result<()> {
        let _lock = self.lock()?;
        let Disk::Sealed { sealed } = self.read_disk()? else {
            anyhow::bail!("the secret store is not sealed");
        };
        let salt = unbase64(&sealed.salt, "salt")?;
        let key = derive_key(password, &salt)?;
        let file = unseal(&sealed, &key)?;
        self.write_disk(&Disk::Plain(file))?;
        MASTERS.lock().unwrap().remove(&self.path);
        Ok(())
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

    /// Every secret, without its value, by name; `root` last, when a
    /// tool holds standing approval for it.
    pub fn list(&self) -> Result<Vec<Listed>> {
        let file = self.load()?;
        let mut listed: Vec<Listed> = file
            .secrets
            .into_iter()
            .map(|(name, entry)| Listed {
                name,
                description: entry.description,
                always: entry.always.into_iter().collect(),
            })
            .collect();
        if !file.root.is_empty() {
            listed.push(Listed {
                name: ROOT.into(),
                description: ROOT_DESCRIPTION.into(),
                always: file.root.into_iter().collect(),
            });
        }
        Ok(listed)
    }

    /// Whether `tool` may run as root without asking.
    pub fn root_granted(&self, tool: &str) -> bool {
        self.load()
            .map(|file| file.root.contains(tool))
            .unwrap_or(false)
    }

    /// Store a secret. A name already there keeps its grants: a rotated
    /// key is the same secret. Returns whether it replaced one.
    pub fn set(&self, name: &str, description: &str, value: &str) -> Result<bool> {
        valid_name(name).map_err(anyhow::Error::msg)?;
        if name == ROOT {
            anyhow::bail!("{ROOT} is not a secret to store: it is what the sudo tool asks for");
        }
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

    /// Grant `tool` the secret for good. Returns whether the secret
    /// exists; `root` always does.
    pub fn grant_always(&self, name: &str, tool: &str) -> Result<bool> {
        self.update(|file| {
            if name == ROOT {
                file.root.insert(tool.to_string());
                return true;
            }
            match file.secrets.get_mut(name) {
                Some(entry) => {
                    entry.always.insert(tool.to_string());
                    true
                }
                None => false,
            }
        })
    }

    /// Drop the standing grants of a secret, for one tool or all.
    /// Returns whether any was dropped.
    pub fn revoke(&self, name: &str, tool: Option<&str>) -> Result<bool> {
        self.update(|file| {
            let grants = if name == ROOT {
                &mut file.root
            } else {
                match file.secrets.get_mut(name) {
                    Some(entry) => &mut entry.always,
                    None => return false,
                }
            };
            match tool {
                Some(tool) => grants.remove(tool),
                None => !std::mem::take(grants).is_empty(),
            }
        })
    }

    /// The value, for ilar's own use: a provider key the config reads.
    /// Nothing that talks to the model calls this.
    pub fn value(&self, name: &str) -> Result<Option<String>> {
        Ok(self.load()?.secrets.remove(name).map(|entry| entry.value))
    }

    /// Whether there is nothing to list. A store that cannot be read,
    /// locked or damaged, is not: what it holds is worth a tool that
    /// says so, and the resolve path is where the cause is reported.
    pub fn is_empty(&self) -> bool {
        self.list().map(|listed| listed.is_empty()).unwrap_or(false)
    }

    /// Whether the person has a store at all. What decides the
    /// `secrets` tool: a file that exists may gain an entry mid-session,
    /// and a tool that appears halfway through a session is worse than
    /// one that says the store is empty.
    pub fn exists(&self) -> bool {
        self.path.exists()
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

/// The person's answer to a prompt: how long, and what they typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub grant: Grant,
    /// Typed into the prompt when it asked for one: sudo's password.
    /// Held in memory for the session, never written anywhere.
    pub password: Option<String>,
}

impl From<Grant> for Approval {
    fn from(grant: Grant) -> Self {
        Self {
            grant,
            password: None,
        }
    }
}

/// A request to use a secret, delivered over [`GrantSender`] with its
/// one-shot reply path. `None` in the reply is a refusal.
#[derive(Debug)]
pub struct GrantPrompt {
    pub session_id: String,
    pub tool_call_id: Option<String>,
    /// The tool asking: one of [`GRANTABLE_TOOLS`].
    pub tool: String,
    pub secret: String,
    pub description: String,
    /// What the tool is about to run with it: the command, verbatim.
    /// The person reads this before saying yes; it is the whole safety.
    pub detail: String,
    /// The prompt should take a password along with the answer: root,
    /// when none is held or stored. Empty means the system needs none.
    pub password_wanted: bool,
    pub reply: oneshot::Sender<Option<Approval>>,
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
    /// A value a tool obtained on its own authority: the sudo tool's
    /// stored password, covered by the command's approval.
    pub(crate) fn new(name: &str, value: String) -> Self {
        Self {
            name: name.to_string(),
            value,
        }
    }

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
    /// Values typed into a prompt for this session: sudo's password.
    /// Shielded and redacted like stored ones, never written.
    held: Arc<Mutex<BTreeMap<String, String>>>,
    prompts: Option<GrantSender>,
    /// What this driver's user does to unlock a sealed store, in a
    /// refusal the lock caused. The core knows whether there is a
    /// prompt channel, not which driver is on the other end.
    unlock_hint: Option<String>,
    /// Whether this session has a sudo tool: without one, the `root`
    /// row in the listing describes something the model cannot do.
    sudo: bool,
    /// What an ask had to admit, for the result of the call that asked:
    /// an Always the store would not keep. Drained by
    /// [`Self::take_notes`].
    notes: Arc<Mutex<Vec<String>>>,
}

/// What a driver that never said how to unlock the store falls back to.
const UNLOCK_HINT: &str = "the user unlocks it with the master password";

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
            held: Arc::new(Mutex::new(BTreeMap::new())),
            prompts: None,
            unlock_hint: None,
            sudo: false,
            notes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_prompts(mut self, sender: GrantSender) -> Self {
        self.prompts = Some(sender);
        self
    }

    /// Whether the session installs the sudo tool (`agent.sudo`): only
    /// then is `root` something the model can ask for, so only then is
    /// it in the listing.
    pub fn with_sudo(mut self, sudo: bool) -> Self {
        self.sudo = sudo;
        self
    }

    /// What the asks in this call had to admit, dropped as it is taken:
    /// the result of the call carries it.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut self.notes.lock().unwrap())
    }

    /// What the driver's user does to unlock a sealed store: "restart
    /// ilar and enter the master password at the start prompt" for the
    /// TUI, "/unlock <master password>" for the gateway. It goes into
    /// every refusal the lock caused.
    pub fn with_unlock_hint(mut self, hint: impl Into<String>) -> Self {
        self.unlock_hint = Some(hint.into());
        self
    }

    fn unlock_hint(&self) -> &str {
        self.unlock_hint.as_deref().unwrap_or(UNLOCK_HINT)
    }

    /// A store error as the model sees it: the lock says how to open
    /// it, anything else is the failure itself. No `secrets:` prefix —
    /// the tool that asked adds its own name.
    fn store_error(&self, error: anyhow::Error) -> String {
        if error.is::<Locked>() || error.is::<Resealed>() {
            format!("{error}: {}", self.unlock_hint())
        } else {
            format!("{error:#}")
        }
    }

    pub fn store(&self) -> &SecretStore {
        &self.store
    }

    pub fn can_ask(&self) -> bool {
        self.prompts.is_some()
    }

    /// The listing the model gets: names, descriptions, standing grants.
    pub fn listing(&self) -> Result<String> {
        let mut listed = match self.store.list() {
            Ok(listed) => listed,
            Err(error) if error.is::<Locked>() || error.is::<Resealed>() => {
                return Ok(format!(
                    "The secret store is sealed and locked for this session; {}.",
                    self.unlock_hint()
                ));
            }
            Err(error) => return Err(error),
        };
        let session = self.session.lock().unwrap();
        // The `root` row belongs to the sudo tool: it goes when there
        // is no such tool, and it comes from a grant given this session
        // as much as from one in the store — the store knows nothing
        // about "yes, this session".
        if self.sudo {
            if !listed.iter().any(|secret| secret.name == ROOT)
                && session.iter().any(|(name, _)| name == ROOT)
            {
                listed.push(Listed {
                    name: ROOT.into(),
                    description: ROOT_DESCRIPTION.into(),
                    always: Vec::new(),
                });
            }
        } else {
            listed.retain(|secret| secret.name != ROOT);
        }
        if listed.is_empty() {
            return Ok(
                "No secrets are stored. The user adds one with: ilar secret set NAME".to_string(),
            );
        }
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
        let mut all = self.store.all().unwrap_or_default();
        all.extend(
            self.held
                .lock()
                .unwrap()
                .iter()
                .map(|(name, value)| Granted::new(name, value.clone())),
        );
        all
    }

    /// A value a tool may take on its own authority — the sudo tool's
    /// password, covered by the command's approval: what was typed this
    /// session first, then the store.
    pub fn held_or_stored(&self, name: &str) -> Result<Option<String>> {
        if let Some(value) = self.held.lock().unwrap().get(name) {
            return Ok(Some(value.clone()));
        }
        self.store.value(name)
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
        let file = self.store.load().map_err(|error| self.store_error(error))?;
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
                self.ask(&request, name, &entry.description, false).await?;
            }
            granted.push(Granted {
                name: name.clone(),
                value: entry.value.clone(),
            });
        }
        Ok(granted)
    }

    /// Approval to run `request.detail` as root, for the sudo tool:
    /// standing, given this session, or asked for now. `reason` is
    /// what the model said it is for.
    pub async fn approve_root(&self, request: Request<'_>, reason: &str) -> Result<(), String> {
        // Known includes "none needed": an empty answer to the prompt
        // is held too, so a passwordless system is asked once.
        let password_known = self
            .held_or_stored(SUDO_PASSWORD)
            .map_err(|error| self.store_error(error))?
            .is_some();
        let granted =
            self.store.root_granted(request.tool) || self.session_granted(ROOT, request.tool);
        // A standing grant with nobody to ask runs on what is known: a
        // stored password, or none, and sudo says if that was wrong.
        if granted && (password_known || !self.can_ask()) {
            return Ok(());
        }
        self.ask(&request, ROOT, reason, !password_known).await
    }

    /// Drop a value typed this session: a sudo password that was
    /// refused, or an empty answer the system turned out to need
    /// something for, so the next ask takes a new one. Returns whether
    /// one was held — a stored value is not this function's to drop,
    /// and the caller says so differently.
    pub fn forget_held(&self, name: &str) -> bool {
        self.held.lock().unwrap().remove(name).is_some()
    }

    fn session_granted(&self, name: &str, tool: &str) -> bool {
        self.session
            .lock()
            .unwrap()
            .contains(&(name.to_string(), tool.to_string()))
    }

    fn unknown(&self, name: &str) -> String {
        if name == ROOT {
            return format!(
                "{ROOT} is not a stored secret: it is what the sudo tool asks for, and only the \
                 sudo tool can ask"
            );
        }
        // Without the `root` row: it is not stored, and `ilar secret
        // set root` — what this line suggests — is refused.
        let known: Vec<String> = self
            .store
            .list()
            .map(|listed| {
                listed
                    .into_iter()
                    .map(|secret| secret.name)
                    .filter(|name| name != ROOT)
                    .collect()
            })
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
        password_wanted: bool,
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
            password_wanted,
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
        let approval = match answer {
            Ok(Some(approval)) => approval,
            Ok(None) => {
                return Err(format!(
                    "the user denied {name} for this {} call",
                    request.tool
                ));
            }
            Err(_) => return Err(unavailable()),
        };
        // Only what was asked for; and an empty answer is an answer,
        // "none needed", held so the question is not put again.
        if password_wanted {
            self.held.lock().unwrap().insert(
                SUDO_PASSWORD.to_string(),
                approval.password.unwrap_or_default(),
            );
        }
        match approval.grant {
            Grant::Once => {}
            Grant::Session => {
                self.session
                    .lock()
                    .unwrap()
                    .insert((name.to_string(), request.tool.to_string()));
            }
            Grant::Always => {
                // The person said yes; a store that cannot be written
                // makes that a session grant rather than a refusal —
                // said out loud, since "always" was the answer and the
                // next session will ask again.
                if self.store.grant_always(name, request.tool).is_err() {
                    self.session
                        .lock()
                        .unwrap()
                        .insert((name.to_string(), request.tool.to_string()));
                    self.notes.lock().unwrap().push(format!(
                        "{name} for {}: the store is unwritable, so it is granted for this \
                         session only",
                        request.tool
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Every value a command's output is redacted of at the source: the
/// ones this call was granted, plus every other stored or held value.
/// A command that echoes a secret it never asked for — a `cat` of a
/// config file, a `env` — must not reach the spill file or the live
/// tail in the clear either.
pub fn redaction_set(secrets: Option<&Secrets>, granted: &[Granted]) -> Vec<Granted> {
    let Some(secrets) = secrets else {
        return granted.to_vec();
    };
    let mut values = granted.to_vec();
    values.extend(
        secrets
            .all()
            .into_iter()
            .filter(|secret| !granted.iter().any(|one| one.name == secret.name)),
    );
    values
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
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.value().len()));
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
        assert!(!store.exists(), "no file until something is stored");
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
        assert!(store.exists());
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

    /// Sealed, the file holds no plaintext; locked, nothing reads it;
    /// unlocked once, every read and write goes through for the rest of
    /// the process; decrypted, it is plain again.
    #[test]
    fn a_master_password_seals_the_store_for_the_process() {
        let (_dir, store) = store();
        store.set("KEY", "the key", "value-one").unwrap();
        assert!(!store.is_sealed());
        assert!(store.encrypt("abc").is_err(), "a short master password");
        assert!(
            store.unlock("whatever").is_err(),
            "nothing sealed to unlock"
        );
        store.encrypt("open sesame").unwrap();
        assert!(store.is_sealed());
        assert!(!store.is_locked(), "the sealer holds the key");
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(raw.contains("\"sealed\""), "{raw}");
        assert!(
            !raw.contains("value-one") && !raw.contains("the key"),
            "{raw}"
        );
        assert!(store.encrypt("again").is_err(), "already sealed");

        // Writes stay sealed, and reads see them.
        store.set("OTHER", "", "value-two").unwrap();
        assert!(
            !std::fs::read_to_string(store.path())
                .unwrap()
                .contains("value-two")
        );
        assert_eq!(store.value("OTHER").unwrap().as_deref(), Some("value-two"));

        // Forget the key: locked, and every read says so.
        MASTERS.lock().unwrap().remove(store.path());
        assert!(store.is_locked());
        let error = store.list().unwrap_err();
        assert!(error.is::<Locked>(), "{error:#}");
        assert!(!store.is_empty(), "a locked store is not nothing");
        assert!(store.set("X", "", "value-x").is_err());
        assert!(store.unlock("wrong").is_err());
        assert!(store.is_locked());
        store.unlock("open sesame").unwrap();
        assert!(!store.is_locked());
        assert_eq!(store.list().unwrap().len(), 2);
        let secrets = Secrets::new(store.clone());
        assert_eq!(secrets.all().len(), 2);

        // Plain again, under the right password only.
        assert!(store.decrypt("wrong").is_err());
        store.decrypt("open sesame").unwrap();
        assert!(!store.is_sealed());
        assert!(
            std::fs::read_to_string(store.path())
                .unwrap()
                .contains("value-one")
        );
        assert!(!MASTERS.lock().unwrap().contains_key(store.path()));
    }

    /// A file that names a seal but gets it wrong is refused, not read
    /// as an empty plain store the next write would replace.
    #[test]
    fn a_malformed_seal_is_refused_not_emptied() {
        let (_dir, store) = store();
        store.set("KEY", "", "value-one").unwrap();
        store.encrypt("open sesame").unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        std::fs::write(store.path(), raw.replace("\"nonce\"", "\"nonce_\"")).unwrap();
        let error = store.list().unwrap_err();
        assert!(format!("{error:#}").contains("malformed"), "{error:#}");
        assert!(!store.is_empty());
        assert!(store.set("X", "", "value-x").is_err());
        assert!(
            std::fs::read_to_string(store.path())
                .unwrap()
                .contains("nonce_"),
            "the file was rewritten"
        );
        // A nonce of the wrong length is refused too.
        let short = raw.replace(&raw[raw.find("\"nonce\": \"").unwrap() + 10..][..8], "");
        std::fs::write(store.path(), short).unwrap();
        assert!(store.list().is_err());
    }

    #[tokio::test]
    async fn a_locked_store_refuses_and_lists_as_locked() {
        let (_dir, store) = store();
        store.set("KEY", "", "value-one").unwrap();
        store.encrypt("open sesame").unwrap();
        MASTERS.lock().unwrap().remove(store.path());
        let secrets = Secrets::new(store.clone());
        assert!(secrets.listing().unwrap().contains("sealed and locked"));
        let cancel = cancel();
        let names = ["KEY".to_string()];
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("locked"), "{error}");
        // With no driver hint, the generic one; with a hint, the way
        // out that driver offers, in the refusal and in the listing.
        assert!(error.contains(UNLOCK_HINT), "{error}");
        assert!(!error.contains("secrets: secrets"), "{error}");
        let hinted = Secrets::new(store.clone()).with_unlock_hint("/unlock <master password>");
        let error = hinted.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("/unlock <master password>"), "{error}");
        assert!(
            hinted
                .listing()
                .unwrap()
                .contains("/unlock <master password>"),
            "{}",
            hinted.listing().unwrap()
        );
    }

    /// A second process reseals the file under another password: the
    /// one this process holds is dropped, the store reads as locked
    /// again, and the refusal says what happened.
    #[tokio::test]
    async fn a_resealed_store_says_so_and_locks_again() {
        let (_dir, store) = store();
        store.set("KEY", "", "value-one").unwrap();
        store.encrypt("open sesame").unwrap();
        let theirs = std::fs::read_to_string(store.path()).unwrap();
        // What a second process leaves behind: the same secrets, sealed
        // under a password this one never saw.
        store.decrypt("open sesame").unwrap();
        store.encrypt("hunter22").unwrap();
        std::fs::write(store.path(), &theirs).unwrap();
        assert!(!store.is_locked(), "a master is still held");
        let secrets = Secrets::new(store.clone()).with_unlock_hint("restart ilar");
        let cancel = cancel();
        let names = ["KEY".to_string()];
        let error = secrets.resolve(request(&names, &cancel)).await.unwrap_err();
        assert!(error.contains("sealed again"), "{error}");
        assert!(error.contains("restart ilar"), "{error}");
        assert!(store.is_locked(), "the useless master was kept");
        store.unlock("open sesame").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
    }

    /// Root is a pseudo-secret: nothing to store, a standing grant to
    /// keep, listed last, revoked by name.
    #[tokio::test]
    async fn root_is_approved_not_stored() {
        let (_dir, store) = store();
        assert!(store.set(ROOT, "", "hunter22").is_err());
        assert!(!store.root_granted("sudo"));
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store.clone()).with_prompts(tx);
        let cancel = cancel();
        let ask = Request {
            tool: "sudo",
            names: &[],
            detail: "apt install ripgrep",
            session_id: "s1",
            tool_call_id: None,
            cancel: &cancel,
        };
        let (outcome, asked) = tokio::join!(
            secrets.approve_root(ask, "to install ripgrep"),
            answer(&mut rx, None)
        );
        assert!(outcome.unwrap_err().contains("denied root"));
        assert_eq!((asked.tool.as_str(), asked.secret.as_str()), ("sudo", ROOT));
        assert_eq!(asked.description, "to install ripgrep");
        assert!(asked.password_wanted, "no password is known yet");
        // A typed password is held for the session and covers the
        // next asks; with a standing grant nothing is asked again.
        let (outcome, _) = tokio::join!(
            secrets.approve_root(ask, ""),
            answer_with(
                &mut rx,
                Some(Approval {
                    grant: Grant::Always,
                    password: Some("hunter22".into()),
                })
            )
        );
        assert!(outcome.is_ok());
        assert!(store.root_granted("sudo"));
        assert_eq!(
            secrets.held_or_stored(SUDO_PASSWORD).unwrap().as_deref(),
            Some("hunter22")
        );
        assert!(
            store.value(SUDO_PASSWORD).unwrap().is_none(),
            "written to disk"
        );
        assert!(secrets.all().iter().any(|held| held.name == SUDO_PASSWORD));
        assert!(secrets.approve_root(ask, "").await.is_ok());
        // A fresh runtime holds no password: the standing grant still
        // asks, for the password alone — and an empty answer is held
        // as "none needed", so it asks once.
        let (tx2, mut rx2) = grant_channel(1);
        let fresh = Secrets::new(store.clone()).with_prompts(tx2);
        let (outcome, asked) = tokio::join!(
            fresh.approve_root(ask, ""),
            answer_with(
                &mut rx2,
                Some(Approval {
                    grant: Grant::Once,
                    password: Some(String::new()),
                })
            )
        );
        assert!(outcome.is_ok());
        assert!(asked.password_wanted);
        assert_eq!(
            fresh.held_or_stored(SUDO_PASSWORD).unwrap().as_deref(),
            Some("")
        );
        assert!(fresh.approve_root(ask, "").await.is_ok(), "asked again");
        // A refused password is forgotten, and the next ask wants one.
        fresh.forget_held(SUDO_PASSWORD);
        let (outcome, asked) = tokio::join!(
            fresh.approve_root(ask, ""),
            answer(&mut rx2, Some(Grant::Once))
        );
        assert!(outcome.is_ok());
        assert!(asked.password_wanted);
        // Headless with a standing grant: runs on what is known.
        assert!(
            Secrets::new(store.clone())
                .approve_root(ask, "")
                .await
                .is_ok()
        );
        let listed = store.list().unwrap();
        assert_eq!(listed.last().unwrap().name, ROOT);
        assert_eq!(listed.last().unwrap().always, ["sudo"]);
        assert!(store.revoke(ROOT, None).unwrap());
        assert!(store.list().unwrap().is_empty());
        let fresh = Secrets::new(store.clone());
        let error = fresh.approve_root(ask, "").await.unwrap_err();
        assert!(
            error.contains("ilar secret grant root --tool sudo"),
            "{error}"
        );
    }

    /// One unknown name fails the call before anyone is asked about
    /// the known ones.
    /// The `root` row is the sudo tool's: it shows a grant given this
    /// session as well as a stored one, and it is absent where there is
    /// no sudo tool to use it.
    #[tokio::test]
    async fn the_root_row_follows_the_sudo_tool() {
        let (_dir, store) = store();
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store.clone()).with_prompts(tx).with_sudo(true);
        let cancel = cancel();
        let ask = Request {
            tool: "sudo",
            names: &[],
            detail: "apt install ripgrep",
            session_id: "s1",
            tool_call_id: None,
            cancel: &cancel,
        };
        assert!(
            secrets
                .listing()
                .unwrap()
                .starts_with("No secrets are stored")
        );
        let (outcome, _) = tokio::join!(
            secrets.approve_root(ask, "to install ripgrep"),
            answer(&mut rx, Some(Grant::Session))
        );
        assert!(outcome.is_ok());
        assert!(store.list().unwrap().is_empty(), "written to the store");
        let listing = secrets.listing().unwrap();
        assert!(listing.contains(ROOT), "{listing}");
        assert!(listing.contains("sudo (this session)"), "{listing}");
        assert!(listing.contains(ROOT_DESCRIPTION), "{listing}");

        // A standing grant in the store, and no sudo tool in the
        // session: nothing can use root, so it is not offered.
        store.grant_always(ROOT, "sudo").unwrap();
        let sudoless = Secrets::new(store.clone());
        assert!(
            sudoless
                .listing()
                .unwrap()
                .starts_with("No secrets are stored"),
            "{}",
            sudoless.listing().unwrap()
        );
        assert!(secrets.listing().unwrap().contains("sudo (always)"));
    }

    /// An "always" the store cannot keep is a session grant, and the
    /// call that asked says so instead of promising it was written.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_always_the_store_cannot_keep_says_it_is_for_the_session() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.set("KEY", "", "value-one").unwrap();
        let (tx, mut rx) = grant_channel(1);
        let secrets = Secrets::new(store.clone()).with_prompts(tx);
        // The state directory read-only: the file still reads, no
        // replacement can be written beside it.
        let state = store.path().parent().unwrap().to_path_buf();
        let was = std::fs::metadata(&state).unwrap().permissions();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
        let cancel = cancel();
        let names = ["KEY".to_string()];
        let (outcome, _) = tokio::join!(
            secrets.resolve(request(&names, &cancel)),
            answer(&mut rx, Some(Grant::Always))
        );
        std::fs::set_permissions(&state, was).unwrap();
        assert!(outcome.is_ok(), "{:?}", outcome.err());
        let notes = secrets.take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("KEY for bash"), "{notes:?}");
        assert!(
            notes[0].contains("granted for this session only"),
            "{notes:?}"
        );
        assert!(secrets.take_notes().is_empty(), "a note was said twice");
        assert!(
            store.list().unwrap()[0].always.is_empty(),
            "the store took it after all"
        );
        // Granted for the session all the same: no second question.
        assert!(secrets.resolve(request(&names, &cancel)).await.is_ok());
    }

    /// Output is redacted of every value the store holds, not only the
    /// ones the call asked for: a command that echoes somebody else's
    /// token never reaches the spill file with it.
    #[test]
    fn the_redaction_set_is_every_stored_value() {
        let (_dir, store) = store();
        store.set("A", "", "aaaa-value").unwrap();
        store.set("B", "", "bbbb-value").unwrap();
        let secrets = Secrets::new(store.clone());
        let granted = vec![Granted {
            name: "A".into(),
            value: "aaaa-value".into(),
        }];
        let values = redaction_set(Some(&secrets), &granted);
        assert_eq!(values.len(), 2);
        assert_eq!(
            redact("aaaa-value bbbb-value", &values),
            "<secret:A> <secret:B>"
        );
        // No store at all: only what the call was handed.
        assert_eq!(redaction_set(None, &granted).len(), 1);
    }

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
        answer_with(rx, grant.map(Approval::from)).await
    }

    async fn answer_with(rx: &mut GrantReceiver, approval: Option<Approval>) -> GrantPrompt {
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
            password_wanted,
            ..
        } = prompt;
        reply.send(approval).unwrap();
        let (dead, _) = oneshot::channel();
        GrantPrompt {
            session_id,
            tool_call_id,
            tool,
            secret,
            description,
            detail,
            password_wanted,
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
