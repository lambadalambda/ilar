//! Experimental, opt-in macOS child-process read/write/network restrictions.
//!
//! File contents are restricted to workspace, runtime/toolchain defaults, and
//! explicit grants. Metadata probes remain allowed outside those roots. This is
//! not full App Sandbox isolation: inherited descriptors and IPC remain outside
//! its protection. Do not use it to run hostile code.
//!
//! Writes are allowed in the canonical workspace, `/dev/null`, explicit absolute
//! `write_paths`, and caches selected by explicit profile names. No shared `/tmp`,
//! per-user temp root, whole home, or whole Library grant is added. `TMPDIR`, `TMP`,
//! and `TEMP` use a checked `.ilar-cache/tmp` below the workspace. Build and tool
//! caches use the same disposable subtree. Cleanup is the caller's responsibility.
//!
//! Paths must be absolute and contain no `..`. Existing symlinks are resolved;
//! missing suffixes are anchored to the canonical nearest existing directory.
//! Dangling symlinks, non-UTF-8 paths, and resolution errors fail closed.
//! Grants are pathname-based, not inode isolation. Path replacement and hard
//! links remain limits; resolution is not a lock on the directory tree.
//!
//! Named profiles add read/write caches under the parent's `HOME`: `cargo` adds registry
//! and git caches plus package-cache locks; `python` adds pip caches; `node` adds
//! npm caches/logs and its update-check marker. They never enable network, install
//! binaries, or infer permissions from commands, dependencies, or error messages.
//! Symlinks below HOME in named cache paths are rejected; use an explicit grant
//! when a cache intentionally lives elsewhere.
//!
//! Seatbelt is deprecated and its SBPL interface is not a stable macOS API.
//! Unlike the initial no-wrapper design, this backend uses the system
//! `/usr/bin/sandbox-exec` helper. Calling `sandbox_init` in a pre-exec hook can
//! allocate or lock after fork and deadlock a multithreaded parent. The helper
//! compiles and applies SBPL after exec instead. Production code uses no FFI or
//! pre-exec hook. The profile is passed as one argument, never through shell text.
//! The helper may print errors and exit nonzero if activation is unavailable;
//! it must not fall back to an unsandboxed shell.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail, ensure};

/// Explicit child-process grants. Disabled by default on every platform.
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub network: bool,
    pub profiles: Vec<String>,
    /// Canonical user skill registrations frozen for this live session only.
    #[serde(skip)]
    pub default_skill_roots: Vec<PathBuf>,
}

impl SandboxConfig {
    pub(crate) fn freeze_default_skill_roots(&mut self, home: Option<&Path>, config_dir: &Path) {
        self.default_skill_roots.clear();
        if !self.enabled {
            return;
        }
        let mut registrations = vec![config_dir.join("skills")];
        let mut containers = vec![
            PathBuf::from("/"),
            PathBuf::from("/Users"),
            PathBuf::from("/home"),
            PathBuf::from("/Library"),
            PathBuf::from("/tmp"),
            PathBuf::from("/var/tmp"),
            PathBuf::from("/var/folders"),
            std::env::temp_dir(),
            config_dir.to_path_buf(),
        ];
        if let Some(home) = home.and_then(|path| resolve_path(path).ok()) {
            registrations.extend([home.join(".agents/skills"), home.join(".claude/skills")]);
            containers.extend(
                [
                    "",
                    ".config",
                    ".local",
                    ".local/share",
                    "Library",
                    ".agents",
                    ".claude",
                    "repos",
                ]
                .map(|suffix| home.join(suffix)),
            );
        }
        let containers: Vec<_> = containers
            .iter()
            .filter_map(|path| resolve_path(path).ok())
            .collect();
        // These registrations are trusted user installation choices, not project
        // paths. Follow the root once, never scan or authorize inner link targets.
        self.default_skill_roots = registrations
            .iter()
            .filter_map(|path| path.canonicalize().ok())
            .filter(|root| {
                root.is_dir()
                    && !containers
                        .iter()
                        .any(|container| container.starts_with(root))
            })
            .collect();
        self.default_skill_roots.sort();
        self.default_skill_roots.dedup();
    }
}

/// In-memory grants shared by calls of one live session, keyed by session id.
/// This store never disables confinement and is not serialized.
#[derive(Clone, Debug, Default)]
pub struct SessionGrants(Arc<Mutex<HashMap<String, SandboxConfig>>>);

impl SessionGrants {
    pub(crate) fn apply(&self, session_id: &str, config: &mut SandboxConfig) {
        let mut grants = self.0.lock().unwrap();
        if let Some(granted) = grants.get_mut(session_id) {
            // Do not let a retained pathname silently redirect after approval.
            // Drop changed/unresolvable roots; a new target needs approval again.
            let unchanged = |path: &PathBuf| resolve_path(path).is_ok_and(|now| now == *path);
            granted.read_paths.retain(unchanged);
            granted.write_paths.retain(unchanged);
            config.read_paths.extend(granted.read_paths.iter().cloned());
            config
                .write_paths
                .extend(granted.write_paths.iter().cloned());
            config.network |= granted.network;
        }
    }

    pub(crate) fn remember(
        &self,
        session_id: &str,
        reads: &[PathBuf],
        writes: &[PathBuf],
        network: bool,
    ) {
        let mut grants = self.0.lock().unwrap();
        let granted = grants.entry(session_id.to_owned()).or_default();
        granted.read_paths.extend_from_slice(reads);
        granted.read_paths.sort();
        granted.read_paths.dedup();
        granted.write_paths.extend_from_slice(writes);
        granted.write_paths.sort();
        granted.write_paths.dedup();
        granted.network |= network;
    }
}

/// Build a shell command prefix. The caller appends `-c` and the script, then
/// sets its environment, stdio, and process-group hook before spawning.
///
/// Disabled configurations return plain `sh`, with no path validation, working
/// directory, arguments, or environment overrides. Enabled configurations fail
/// on non-macOS systems. On macOS the prefix is `/usr/bin/sandbox-exec -p PROFILE
/// /bin/sh`, with canonical `cwd` and workspace-scoped temporary-file variables.
/// Do not override these temporary-file variables unless explicitly intended.
/// Activation failures are helper exit failures with stderr, not spawn errors.
/// A missing helper is a spawn error. Neither case falls back to plain `sh`.
pub fn command(cwd: &Path, config: &SandboxConfig) -> anyhow::Result<tokio::process::Command> {
    if !config.enabled {
        return Ok(tokio::process::Command::new("sh"));
    }
    sandbox_command(cwd, config, Path::new("/bin/sh"), false)
}

/// Launch an installed helper with the same policy as shell tools. Only the
/// executable itself gets an extra read grant, not its installation directory.
pub(crate) fn program(
    cwd: &Path,
    config: &SandboxConfig,
    executable: &Path,
) -> anyhow::Result<tokio::process::Command> {
    if !config.enabled {
        return Ok(tokio::process::Command::new(executable));
    }
    sandbox_command(cwd, config, executable, true)
}

fn sandbox_command(
    cwd: &Path,
    config: &SandboxConfig,
    executable: &Path,
    grant_executable: bool,
) -> anyhow::Result<tokio::process::Command> {
    #[cfg(target_os = "macos")]
    {
        let workspace = resolve_path(cwd)?;
        ensure!(
            workspace.is_dir(),
            "sandbox workspace must be an existing directory"
        );
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut profile = build_profile(&workspace, config, home.as_deref())?;
        let executable = if grant_executable {
            let executable = resolve_path(executable)?;
            ensure!(executable.is_file(), "sandbox helper is not a file");
            profile.push_str(&format!(
                "\n(allow file-read* (literal {}))\n",
                sbpl_path(&executable)?
            ));
            executable
        } else {
            executable.to_path_buf()
        };
        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command.args(["-p", &profile]).arg(executable);
        command.current_dir(&workspace);
        apply_environment(&mut command, &workspace)?;
        Ok(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cwd, config, executable, grant_executable);
        anyhow::bail!("experimental process sandbox is supported only on macOS")
    }
}

/// Preserve inherited version precedence; let mise dispatch from project config.
/// Only insert native Apple tools to avoid their cache-writing launchers.
#[cfg(any(target_os = "macos", test))]
fn direct_tool_path(path: &std::ffi::OsStr, home: &Path, native: &[PathBuf]) -> std::ffi::OsString {
    let inherited: Vec<_> = std::env::split_paths(path).collect();
    let shims = home.join(".local/share/mise/shims");
    let Some(index) = inherited.iter().position(|path| path == &shims) else {
        return path.to_owned();
    };
    let mut preferred: Vec<_> = inherited[..index].to_vec();
    preferred.extend(native.iter().cloned());
    for path in &inherited[index..] {
        if !preferred.contains(path) {
            preferred.push(path.clone());
        }
    }
    std::env::join_paths(preferred).unwrap_or_else(|_| path.to_owned())
}

#[cfg(any(target_os = "macos", test))]
fn direct_compiler(
    value: &std::ffi::OsStr,
    home: &Path,
    path: &std::ffi::OsStr,
) -> Option<PathBuf> {
    let value = Path::new(value);
    let shims = home.join(".local/share/mise/shims");
    if value.parent()? != shims {
        return None;
    }
    let name = value.file_name()?;
    std::env::split_paths(path)
        .filter(|bin| bin != &shims)
        .map(|bin| bin.join(name))
        .find(|tool| tool.is_file())
}

/// Create below an open workspace descriptor. Never follow repository-controlled
/// links while the parent still has its own filesystem permissions.
#[cfg(target_os = "macos")]
fn create_cache_directories(workspace: &Path, suffixes: &[&str]) -> anyhow::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let workspace = std::ffi::CString::new(workspace.as_os_str().as_bytes())?;
    // SAFETY: the path is NUL-terminated; each successful fd is owned once.
    let fd = unsafe { libc::open(workspace.as_ptr(), flags) };
    ensure!(
        fd >= 0,
        "open sandbox workspace: {}",
        std::io::Error::last_os_error()
    );
    let workspace = unsafe { OwnedFd::from_raw_fd(fd) };
    for suffix in suffixes {
        let mut parent = workspace.try_clone()?;
        for part in std::iter::once(".ilar-cache").chain(suffix.split('/')) {
            let part = std::ffi::CString::new(part)?;
            // SAFETY: parent stays open for both descriptor-relative calls;
            // fixed single components contain neither slashes nor parent steps.
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), part.as_ptr(), 0o700) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error).context("create sandbox cache directory");
                }
            }
            let fd = unsafe { libc::openat(parent.as_raw_fd(), part.as_ptr(), flags) };
            ensure!(
                fd >= 0,
                "open sandbox cache directory without symlinks: {}",
                std::io::Error::last_os_error()
            );
            parent = unsafe { OwnedFd::from_raw_fd(fd) };
        }
    }
    Ok(())
}

/// Only called for enabled policies, after policy and workspace validation.
#[cfg(target_os = "macos")]
fn apply_environment(
    command: &mut tokio::process::Command,
    workspace: &Path,
) -> anyhow::Result<()> {
    let cache = workspace.join(".ilar-cache");
    let variables = [
        ("TMPDIR", "tmp"),
        ("TMP", "tmp"),
        ("TEMP", "tmp"),
        ("GOCACHE", "go-build"),
        ("XDG_CACHE_HOME", "xdg"),
        ("npm_config_cache", "npm"),
        ("CLANG_MODULE_CACHE_PATH", "xdg/clang"),
        ("MISE_CACHE_DIR", "xdg/mise-cache"),
        ("MISE_STATE_DIR", "xdg/mise-state"),
        ("MISE_CONFIG_DIR", "xdg/mise-config"),
    ];
    create_cache_directories(workspace, &variables.map(|(_, suffix)| suffix))?;
    for (key, suffix) in variables {
        command.env(key, cache.join(suffix));
    }
    if let (Some(home), Some(path)) = (std::env::var_os("HOME"), std::env::var_os("PATH")) {
        let home = resolve_path(Path::new(&home))?;
        // Use the selected native compiler/linker together. LLVM's installed
        // linker can lag the active macOS SDK. Direct Xcode tools also avoid
        // xcrun's /var/folders lookup cache (even --no-cache tries to write it).
        let developer = std::env::var_os("DEVELOPER_DIR")
            .map(PathBuf::from)
            .or_else(|| std::fs::read_link("/var/db/xcode_select_link").ok())
            .or_else(|| {
                Path::new("/Library/Developer/CommandLineTools")
                    .is_dir()
                    .then(|| PathBuf::from("/Library/Developer/CommandLineTools"))
            });
        if std::env::var_os("SDKROOT").is_none()
            && let Some(dir) = &developer
        {
            for sdk in [
                dir.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"),
                dir.join("SDKs/MacOSX.sdk"),
            ] {
                if let Ok(sdk) = sdk.canonicalize()
                    && sdk.is_dir()
                    && (sdk.starts_with("/Applications/Xcode.app")
                        || sdk.starts_with("/Library/Developer"))
                {
                    command.env("SDKROOT", sdk);
                    break;
                }
            }
        }
        let native: Vec<_> = developer
            .into_iter()
            .flat_map(|dir| {
                [
                    dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin"),
                    dir.join("usr/bin"),
                ]
            })
            .filter_map(|dir| resolve_path(&dir).ok())
            .filter(|dir| {
                dir.is_dir()
                    && (dir.starts_with("/Applications/Xcode.app")
                        || dir.starts_with("/Library/Developer"))
            })
            .collect();
        let path = direct_tool_path(&path, &home, &native);
        for key in ["CC", "CXX"] {
            if let Some(value) = std::env::var_os(key)
                && let Some(compiler) = direct_compiler(&value, &home, &path)
            {
                command.env(key, compiler);
            }
        }
        command.env("PATH", path);
    }
    // Do not read global mise configuration, which can contain credentials and
    // hooks. Shell-level assignments can still override these for an approved call.
    command
        .env(
            "MISE_GLOBAL_CONFIG_FILE",
            cache.join("xdg/mise-config/config.toml"),
        )
        .env(
            "MISE_SYSTEM_CONFIG_FILE",
            cache.join("xdg/mise-config/system.toml"),
        )
        .env("MISE_TRUSTED_CONFIG_PATHS", workspace)
        .env_remove("__MISE_DIFF")
        .env_remove("__MISE_ORIG_PATH")
        .env_remove("__MISE_SESSION");
    Ok(())
}

pub(crate) fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "sandbox path must be absolute: {}",
        path.display()
    );
    ensure!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "sandbox path must not contain '..': {}",
        path.display()
    );
    let mut ancestor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                if !missing.is_empty() {
                    ensure!(
                        resolved.is_dir(),
                        "sandbox path ancestor is not a directory: {}",
                        ancestor.display()
                    );
                }
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // canonicalize reports NotFound for dangling links too. Do not
                // mistake those for directories that the child can create.
                match std::fs::symlink_metadata(&ancestor) {
                    Ok(_) => bail!("sandbox path cannot be resolved: {}", ancestor.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e).context("inspect sandbox path"),
                }
                missing.push(
                    ancestor
                        .file_name()
                        .context("sandbox path has no existing ancestor")?
                        .to_os_string(),
                );
                ensure!(ancestor.pop(), "sandbox path has no existing ancestor");
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolve sandbox path {}", path.display()));
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn sbpl_path(path: &Path) -> anyhow::Result<String> {
    sbpl_string(path.to_str().context("sandbox paths must be UTF-8")?)
}

#[cfg(any(target_os = "macos", test))]
fn sbpl_string(value: &str) -> anyhow::Result<String> {
    let mut escaped = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => bail!("unsupported control character in sandbox path"),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

/// Paths used by both the Seatbelt policy and redundant-request coverage.
/// A write grant does not imply a read grant.
#[derive(Default)]
pub(crate) struct AccessRoots {
    reads: Vec<PathRoot>,
    writes: Vec<PathRoot>,
}

#[derive(Clone)]
struct PathRoot {
    path: PathBuf,
    subtree: bool,
}

impl PathRoot {
    fn new(path: impl Into<PathBuf>, subtree: bool) -> Self {
        Self {
            path: path.into(),
            subtree,
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn filter(&self) -> anyhow::Result<String> {
        Ok(format!(
            "({} {})",
            if self.subtree { "subpath" } else { "literal" },
            sbpl_path(&self.path)?
        ))
    }
}

impl AccessRoots {
    pub(crate) fn covers(&self, paths: &[PathBuf], write: bool) -> bool {
        let roots = if write { &self.writes } else { &self.reads };
        paths.iter().all(|path| {
            roots.iter().any(|root| {
                if root.subtree {
                    path.starts_with(&root.path)
                } else {
                    path == &root.path && !path.is_dir()
                }
            })
        })
    }
}

pub(crate) fn access_roots(cwd: &Path, config: &SandboxConfig) -> anyhow::Result<AccessRoots> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config_dir = std::env::var_os("ILAR_CONFIG_DIR").map(PathBuf::from);
    policy_roots(
        &resolve_path(cwd)?,
        config,
        home.as_deref(),
        config_dir.as_deref(),
    )
}

// Optional defaults must not make unrelated commands fail or follow redirected
// user directories into a broader tree. Explicit grants can authorize a target.
fn optional_root(expected: &Path) -> Option<PathBuf> {
    resolve_path(expected)
        .ok()
        .filter(|resolved| resolved == expected)
}

fn policy_roots(
    cwd: &Path,
    config: &SandboxConfig,
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> anyhow::Result<AccessRoots> {
    let mut roots = AccessRoots {
        reads: vec![PathRoot::new(cwd, true), PathRoot::new("/dev/null", false)],
        writes: vec![PathRoot::new(cwd, true), PathRoot::new("/dev/null", false)],
    };
    for path in &config.default_skill_roots {
        // A retained canonical pathname must not gain access to a replacement
        // symlink target. Missing or redirected roots are not session defaults.
        if optional_root(path).is_some() && path.is_dir() {
            roots.reads.push(PathRoot::new(path, true));
        }
    }
    for path in &config.write_paths {
        roots.writes.push(PathRoot::new(resolve_path(path)?, true));
    }
    for path in &config.read_paths {
        roots.reads.push(PathRoot::new(resolve_path(path)?, true));
    }
    for name in &config.profiles {
        let (subtrees, files): (&[&str], &[&str]) = match name.as_str() {
            "cargo" => (
                &[".cargo/registry", ".cargo/git"],
                &[".cargo/.package-cache", ".cargo/.package-cache-mutate"],
            ),
            "python" => (&[".cache/pip", "Library/Caches/pip"], &[]),
            "node" => (
                &[".npm/_cacache", ".npm/_logs"],
                &[".npm/_update-notifier-last-checked"],
            ),
            _ => bail!("unknown sandbox profile: {name}"),
        };
        let home = resolve_path(home.context("sandbox cache profiles require an absolute HOME")?)?;
        for (subtree, paths) in [(true, subtrees), (false, files)] {
            for path in paths {
                let expected = home.join(path);
                let resolved = resolve_path(&expected)?;
                ensure!(
                    resolved == expected,
                    "sandbox cache path redirects through a symlink: {}; use an explicit write_paths grant instead",
                    expected.display()
                );
                let root = PathRoot::new(resolved, subtree);
                roots.reads.push(root.clone());
                roots.writes.push(root);
            }
        }
    }
    // Runtime and installed-tool data, not arbitrary user configuration.
    // APPROXIMATION: this macOS baseline covers common toolchains; which
    // additional paths do other installations need? Extend only from probes.
    for path in [
        "/System",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/usr/share",
        "/usr/local/bin",
        "/usr/local/lib",
        "/usr/local/opt",
        "/usr/local/Cellar",
        "/usr/local/share",
        "/usr/local/Homebrew/Library/Homebrew/os/mac/pkgconfig",
        "/Library/Apple",
        "/Library/Developer",
        "/Applications/Xcode.app",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/opt/homebrew/lib",
        "/opt/homebrew/opt",
        "/opt/homebrew/Cellar",
        "/opt/homebrew/share",
        "/opt/homebrew/Library/Homebrew/os/mac/pkgconfig",
        "/private/var/db/dyld",
        "/private/var/db/timezone",
        "/private/etc/ssl",
    ] {
        roots.reads.push(PathRoot::new(path, true));
    }
    // dyld probes the root directory on current macOS. Literal, not subpath:
    // root enumeration must not grant access to every file below it.
    roots.reads.push(PathRoot::new("/", false));
    for path in [
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/private/etc/hosts",
        "/private/etc/resolv.conf",
        "/private/etc/localtime",
        "/private/etc/passwd",
        "/private/etc/group",
        "/private/etc/services",
        "/private/etc/protocols",
        "/Library/Preferences/com.apple.dt.Xcode.plist",
        "/opt/homebrew/etc/ca-certificates/cert.pem",
    ] {
        roots.reads.push(PathRoot::new(path, false));
    }
    if let Some(home) = home {
        let home = resolve_path(home)?;
        for path in [
            ".cargo/bin",
            ".rustup/toolchains",
            ".rustup/settings.toml",
            ".local/share/mise/installs",
            ".local/share/mise/shims",
            "go/pkg/mod", // Includes cache/download metadata; never writable by default.
            ".agents/skills",
            ".claude/skills",
        ] {
            if let Some(path) = optional_root(&home.join(path)) {
                roots.reads.push(PathRoot::new(path, true));
            }
        }
        if let Some(path) = optional_root(&home.join(".local/bin/mise")) {
            roots.reads.push(PathRoot::new(path, false));
        }
        if config_dir.is_none()
            && let Some(path) = optional_root(&home.join(".config/ilar/skills"))
        {
            roots.reads.push(PathRoot::new(path, true));
        }
    }
    if let Some(config_dir) = config_dir.and_then(|dir| resolve_path(dir).ok())
        && let Some(path) = optional_root(&config_dir.join("skills"))
    {
        roots.reads.push(PathRoot::new(path, true));
    }
    Ok(roots)
}

#[cfg(any(target_os = "macos", test))]
fn build_profile(
    cwd: &Path,
    config: &SandboxConfig,
    home: Option<&Path>,
) -> anyhow::Result<String> {
    let config_dir = std::env::var_os("ILAR_CONFIG_DIR").map(PathBuf::from);
    let roots = policy_roots(cwd, config, home, config_dir.as_deref())?;
    let filters = |roots: &[PathRoot]| -> anyhow::Result<String> {
        let mut filters = roots
            .iter()
            .map(PathRoot::filter)
            .collect::<anyhow::Result<Vec<_>>>()?;
        filters.sort();
        filters.dedup();
        Ok(filters.join("\n  "))
    };
    let network = if config.network {
        ""
    } else {
        "(deny network*)\n"
    };
    Ok(format!(
        "(version 1)\n(allow default)\n(deny file-read*)\n(allow file-read-metadata)\n(allow file-read*\n  {})\n(deny file-write*)\n(allow file-write*\n  {})\n{network}",
        filters(&roots.reads)?,
        filters(&roots.writes)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn linked_user_skill_index_is_read_only_and_does_not_authorize_link_targets() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let home = root.join("home");
        let config_dir = home.join(".config/ilar");
        let registration = home.join(".agents/skills");
        let target = home.join("repos/skills-and-commands/skills");
        let private = root.join("private");
        let workspace = root.join("workspace");
        for path in [
            target.join("modern-web-guidance"),
            registration.parent().unwrap().to_owned(),
            private.clone(),
            workspace.clone(),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(target.join("modern-web-guidance/INDEX.md"), "reference").unwrap();
        std::fs::write(private.join("secret"), "private").unwrap();
        std::os::unix::fs::symlink(&target, &registration).unwrap();
        std::os::unix::fs::symlink(&private, target.join("inner-link")).unwrap();
        let mut config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        config.freeze_default_skill_roots(Some(&home), &config_dir);
        let output = command(&workspace, &config)
            .unwrap()
            .args([
                "-c",
                r#"
            set -eu
            cat "$1/modern-web-guidance/INDEX.md"
            if (printf bad > "$1/write-probe") 2>/dev/null; then exit 41; fi
            if cat "$1/inner-link/secret" 2>/dev/null; then exit 42; fi
        "#,
                "skill-test",
            ])
            .arg(&registration)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"reference");
        // A changed registration cannot add a new target to this session.
        std::fs::remove_file(&registration).unwrap();
        std::os::unix::fs::symlink(&private, &registration).unwrap();
        let output = command(&workspace, &config)
            .unwrap()
            .args(["-c", "cat \"$1/secret\"", "skill-test"])
            .arg(&registration)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        // A redirected retained canonical root is excluded by policy and coverage.
        std::fs::rename(&target, root.join("old-skills")).unwrap();
        std::os::unix::fs::symlink(&private, &target).unwrap();
        let roots = access_roots(&workspace, &config).unwrap();
        assert!(!roots.covers(&[resolve_path(&target.join("secret")).unwrap()], false));
        let output = command(&workspace, &config)
            .unwrap()
            .args(["-c", "cat \"$1/secret\"", "skill-test"])
            .arg(&target)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires macOS sandbox-exec, mise shims and two installed Go versions"]
    async fn macos_shim_only_path_keeps_project_pinned_go_version() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let installs = home.join(".local/share/mise/installs/go");
        let latest = installs.join("latest").canonicalize().unwrap();
        let pinned = std::fs::read_dir(&installs)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|t| t.is_dir()) && entry.path() != latest)
            .unwrap();
        let version = pinned.file_name().to_str().unwrap().to_owned();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("mise.toml"),
            format!("[tools]\ngo = '{version}'\n"),
        )
        .unwrap();
        let shims = home.join(".local/share/mise/shims");
        let inherited =
            std::env::join_paths([shims.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap();
        let adapted = direct_tool_path(&inherited, &home, &[]);
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let output = command(workspace.path(), &config)
            .unwrap()
            .env("PATH", adapted)
            .args(["-c", "go version"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("go{version} ")),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_skill_registrations_freeze_narrow_canonical_roots() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let config_dir = home.join(".config/ilar");
        let target = home.join("repos/skills-and-commands/skills");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        std::os::unix::fs::symlink(&target, home.join(".agents/skills")).unwrap();
        let mut config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        config.freeze_default_skill_roots(Some(&home), &config_dir);
        let roots = policy_roots(
            &root.path().join("workspace"),
            &config,
            Some(&home),
            Some(&config_dir),
        )
        .unwrap();
        assert!(roots.covers(&[target.join("modern-web-guidance/INDEX.md")], false));
        assert!(!roots.covers(&[target.join("modern-web-guidance/INDEX.md")], true));
        assert!(
            serde_json::to_value(&config)
                .unwrap()
                .get("default_skill_roots")
                .is_none()
        );
        assert!(
            serde_json::from_value::<SandboxConfig>(
                serde_json::json!({"default_skill_roots":["/"]})
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn broad_or_missing_user_skill_targets_are_not_defaults() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let config_dir = home.join(".config/ilar");
        for path in [
            home.join(".agents"),
            config_dir.clone(),
            home.join(".local"),
            home.join("Library"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        let registration = home.join(".agents/skills");
        for target in [
            PathBuf::from("/"),
            home.clone(),
            home.join(".config"),
            config_dir.clone(),
            home.join(".local"),
            home.join("Library"),
            std::env::temp_dir(),
            root.path().join("missing"),
        ] {
            std::os::unix::fs::symlink(&target, &registration).unwrap();
            let mut config = SandboxConfig {
                enabled: true,
                ..Default::default()
            };
            config.freeze_default_skill_roots(Some(&home), &config_dir);
            assert!(
                config.default_skill_roots.is_empty(),
                "{}",
                target.display()
            );
            std::fs::remove_file(&registration).unwrap();
        }
    }

    #[test]
    fn configuration_is_opt_in_and_has_serde_defaults() {
        let config: SandboxConfig = serde_json::from_str("{}").unwrap();
        assert!(!config.enabled);
        assert!(!config.network);
        assert!(config.write_paths.is_empty());
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn disabled_does_not_validate_paths_or_profiles() {
        let config = SandboxConfig {
            profiles: vec!["not-a-profile".into()],
            ..Default::default()
        };
        let child = command(Path::new("does-not-exist"), &config).unwrap();
        let child = child.as_std();
        assert_eq!(child.get_program(), "sh");
        assert_eq!(child.get_args().count(), 0);
        assert_eq!(child.get_envs().count(), 0);
        assert!(child.get_current_dir().is_none());
    }

    #[test]
    fn disabled_program_preserves_executable_without_validating_paths() {
        let child = program(
            Path::new("missing"),
            &SandboxConfig::default(),
            Path::new("/missing/worker"),
        )
        .unwrap();
        assert_eq!(child.as_std().get_program(), "/missing/worker");
        assert_eq!(child.as_std().get_args().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn helper_executable_grant_does_not_grant_its_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let installation = tempfile::tempdir().unwrap();
        let executable = installation.path().join("helper");
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&executable, "#!/bin/sh\nexec /bin/cat \"$@\"\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret = installation.path().join("secret");
        std::fs::write(&secret, "not-readable").unwrap();
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let output = program(workspace.path(), &config, &executable)
            .unwrap()
            .arg(&secret)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"),
            "status: {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn resolves_missing_grants_but_rejects_relative_paths_and_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(
            resolve_path(&dir.path().join("new/cache")).unwrap(),
            canonical.join("new/cache")
        );
        assert!(resolve_path(Path::new("relative/cache")).is_err());
        assert!(resolve_path(&dir.path().join("missing/../cache")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_symlinks_and_rejects_dangling_links() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dangling"))
            .unwrap();
        assert_eq!(
            resolve_path(&dir.path().join("link/new")).unwrap(),
            target.canonicalize().unwrap().join("new")
        );
        assert!(resolve_path(&dir.path().join("dangling/new")).is_err());
    }

    #[test]
    fn escapes_profile_strings_and_rejects_nul() {
        assert_eq!(
            sbpl_string("/a/\"b\\c\n\r\t").unwrap(),
            "\"/a/\\\"b\\\\c\\n\\r\\t\""
        );
        assert!(sbpl_string("/bad\0path").is_err());
        assert!(sbpl_string("/bad\u{1b}path").is_err());
    }

    #[test]
    fn profile_limits_writes_and_denies_network_unless_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let profile = build_profile(&cwd, &config, None).unwrap();
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains(&format!("(subpath {})", sbpl_path(&cwd).unwrap())));
        assert!(profile.contains("(literal \"/dev/null\")"));
        assert!(!profile.contains("(subpath \"/tmp\")"));
        config.network = true;
        assert!(
            !build_profile(&cwd, &config, None)
                .unwrap()
                .contains("(deny network*)")
        );
    }

    #[test]
    fn defaults_read_go_modules_and_skill_references_without_shared_writes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().canonicalize().unwrap().join("home");
        let profile = build_profile(&workspace, &SandboxConfig::default(), Some(&home)).unwrap();
        let (reads, writes) = profile.split_once("(deny file-write*)").unwrap();
        for path in [
            "go/pkg/mod",
            ".config/ilar/skills",
            ".agents/skills",
            ".claude/skills",
        ] {
            let path = sbpl_path(&home.join(path)).unwrap();
            assert!(reads.contains(&path), "missing read root: {path}");
            assert!(!writes.contains(&path), "shared write root: {path}");
        }
        assert!(!reads.contains(&sbpl_path(&home.join(".config/mise")).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn mise_path_preserves_inherited_version_precedence() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let installs = home.join(".local/share/mise/installs");
        let shims = home.join(".local/share/mise/shims");
        let selected = installs.join("go/1/bin");
        let latest = installs.join("go/2/bin");
        let sole = installs.join("node/24/bin");
        for path in [&selected, &latest, &sole] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink("2", installs.join("go/latest")).unwrap();
        let inherited = std::env::join_paths([
            Path::new("/custom/bin"),
            &shims,
            &selected,
            Path::new("/usr/bin"),
        ])
        .unwrap();
        let path: Vec<_> =
            std::env::split_paths(&direct_tool_path(&inherited, &home, &[])).collect();
        assert_eq!(
            path,
            [
                PathBuf::from("/custom/bin"),
                shims,
                selected,
                PathBuf::from("/usr/bin")
            ]
        );
    }

    #[test]
    fn absolute_mise_compiler_shims_are_replaced_but_custom_compilers_are_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap();
        let bin = home.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("clang"), "tool").unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        let shim = home.join(".local/share/mise/shims/clang");
        assert_eq!(
            direct_compiler(shim.as_os_str(), &home, &path),
            Some(bin.join("clang"))
        );
        for value in ["clang", "/custom/clang", "/custom/clang -flag"] {
            assert_eq!(
                direct_compiler(std::ffi::OsStr::new(value), &home, &path),
                None
            );
        }
    }

    #[test]
    fn trusted_config_directory_only_adds_its_skill_subtree() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let home = root.join("home");
        let config = root.join("custom-config");
        let roots = policy_roots(
            &root.join("workspace"),
            &SandboxConfig::default(),
            Some(&home),
            Some(&config),
        )
        .unwrap();
        assert!(roots.covers(&[config.join("skills/reference.md")], false));
        for path in [
            config.join("tokens.toml"),
            home.join(".config/ilar/skills/file"),
        ] {
            assert!(!roots.covers(&[path], false));
        }
        assert!(!roots.covers(&[config.join("skills/reference.md")], true));
    }

    #[cfg(unix)]
    #[test]
    fn redirected_go_and_skill_roots_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let outside = root.path().canonicalize().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        for suffix in [
            "go/pkg/mod",
            ".agents/skills",
            ".claude/skills",
            ".config/ilar/skills",
        ] {
            let path = home.join(suffix);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
        }
        let roots = policy_roots(
            &root.path().join("workspace"),
            &SandboxConfig::default(),
            Some(&home),
            None,
        )
        .unwrap();
        assert!(!roots.covers(&[outside.join("file")], false));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_does_not_disable_git_identity_or_signing_configuration() {
        let workspace = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let child = command(workspace.path(), &config).unwrap();
        assert!(
            !child
                .as_std()
                .get_envs()
                .any(|(key, _)| key.to_string_lossy().starts_with("GIT_CONFIG_"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disposable_environment_is_local_and_not_the_module_root() {
        let workspace = tempfile::tempdir().unwrap();
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let child = command(workspace.path(), &config).unwrap();
        let root = workspace.path().canonicalize().unwrap().join(".ilar-cache");
        for (key, suffix) in [
            ("TMPDIR", "tmp"),
            ("TMP", "tmp"),
            ("TEMP", "tmp"),
            ("GOCACHE", "go-build"),
            ("XDG_CACHE_HOME", "xdg"),
            ("npm_config_cache", "npm"),
        ] {
            let expected = root.join(suffix);
            assert!(
                child
                    .as_std()
                    .get_envs()
                    .any(|(k, v)| k == key && v == Some(expected.as_os_str())),
                "{key}"
            );
            assert!(expected.is_dir(), "{key} directory");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disposable_environment_rejects_redirects_without_creating_external_directories() {
        let outside = tempfile::tempdir().unwrap();
        for suffix in [
            "",
            "tmp",
            "go-build",
            "xdg",
            "npm",
            "xdg/mise-state",
            "xdg/clang",
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let cache = workspace.path().join(".ilar-cache");
            if !suffix.is_empty() {
                std::fs::create_dir_all(cache.join(suffix).parent().unwrap()).unwrap();
            }
            std::os::unix::fs::symlink(
                outside.path(),
                if suffix.is_empty() {
                    cache.clone()
                } else {
                    cache.join(suffix)
                },
            )
            .unwrap();
            let config = SandboxConfig {
                enabled: true,
                ..Default::default()
            };
            assert!(command(workspace.path(), &config).is_err(), "{suffix}");
        }
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn named_profiles_grant_only_explicit_caches() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let home = cwd.join("home");
        let config = SandboxConfig {
            enabled: true,
            profiles: vec!["cargo".into(), "python".into(), "node".into()],
            ..Default::default()
        };
        let profile = build_profile(&cwd, &config, Some(&home)).unwrap();
        for cache in [
            ".cargo/registry",
            ".cargo/git",
            ".cache/pip",
            "Library/Caches/pip",
            ".npm/_cacache",
        ] {
            assert!(
                profile.contains(&sbpl_path(&home.join(cache)).unwrap()),
                "{cache}"
            );
        }
        assert!(!profile.contains(&format!(
            "(subpath {})",
            sbpl_path(&home.join("Library")).unwrap()
        )));
        assert!(!profile.contains(&format!(
            "(subpath {})",
            sbpl_path(&home.join(".local/bin")).unwrap()
        )));
        assert!(profile.contains("(deny network*)"));
        let unknown = SandboxConfig {
            profiles: vec!["unknown".into()],
            ..config
        };
        assert!(build_profile(&cwd, &unknown, Some(&home)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_profiles_do_not_follow_links_into_broader_locations() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir(home.join(".cargo")).unwrap();
        std::fs::create_dir(home.join("Library")).unwrap();
        std::os::unix::fs::symlink(home.join("Library"), home.join(".cargo/registry")).unwrap();
        let config = SandboxConfig {
            profiles: vec!["cargo".into()],
            ..Default::default()
        };
        assert!(build_profile(&home, &config, Some(&home)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn redirected_default_toolchains_require_an_explicit_read_grant() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let tools = root.path().join("tools");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir_all(home.join(".rustup")).unwrap();
        std::fs::create_dir(&tools).unwrap();
        std::os::unix::fs::symlink(&tools, home.join(".rustup/toolchains")).unwrap();
        let grant = format!(
            "(subpath {})",
            sbpl_path(&tools.canonicalize().unwrap()).unwrap()
        );
        let mut config = SandboxConfig::default();
        let profile = build_profile(&workspace, &config, Some(&home)).unwrap();
        assert!(!profile.contains(&grant));
        config.read_paths.push(tools);
        assert!(
            build_profile(&workspace, &config, Some(&home))
                .unwrap()
                .contains(&grant)
        );
    }

    #[test]
    fn configured_grants_do_not_include_parents_or_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cwd = root.join("workspace");
        let grant = root.join("external/cache\"\\\n");
        let config = SandboxConfig {
            write_paths: vec![grant.clone()],
            ..Default::default()
        };
        let profile = build_profile(&cwd, &config, None).unwrap();
        assert!(profile.contains(&format!("(subpath {})", sbpl_path(&grant).unwrap())));
        for excluded in [
            &root,
            &root.join("external"),
            &root.join("external/cache-sibling"),
        ] {
            assert!(!profile.contains(&format!("(subpath {})", sbpl_path(excluded).unwrap())));
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_grants_and_non_directory_ancestors() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let config = SandboxConfig {
            write_paths: vec![cwd.join(std::ffi::OsString::from_vec(vec![0xff]))],
            ..Default::default()
        };
        assert!(build_profile(&cwd, &config, None).is_err());
        std::fs::write(cwd.join("file"), "").unwrap();
        assert!(resolve_path(&cwd.join("file/child")).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn enabled_fails_on_unsupported_platforms() {
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(command(Path::new("/"), &config).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_command_prefix_keeps_shell_script_separate_from_profile() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let mut child = command(dir.path(), &config).unwrap();
        child.args(["-c", "printf '%s' \"quoted script\""]);
        let child = child.as_std();
        assert_eq!(child.get_program(), "/usr/bin/sandbox-exec");
        let args: Vec<_> = child.get_args().collect();
        assert_eq!(args[0], "-p");
        assert!(args[1].to_str().unwrap().contains("(deny network*)"));
        assert_eq!(args[2], "/bin/sh");
        assert_eq!(args[3], "-c");
        assert_eq!(args[4], "printf '%s' \"quoted script\"");
        assert_eq!(child.get_current_dir(), Some(cwd.as_path()));
        for key in ["TMPDIR", "TMP", "TEMP"] {
            assert!(
                child
                    .get_envs()
                    .any(|(k, v)| k == key && v == Some(cwd.join(".ilar-cache/tmp").as_os_str()))
            );
        }
        let invalid = SandboxConfig {
            write_paths: vec!["relative".into()],
            ..config
        };
        assert!(command(dir.path(), &invalid).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_compiles_generated_sbpl_before_any_child_is_forked() {
        // Test-only use of the private compiler checks real SBPL syntax even
        // inside a host sandbox which refuses to apply a second policy.
        #[link(name = "sandbox")]
        unsafe extern "C" {
            fn sandbox_compile_string(
                profile: *const std::ffi::c_char,
                params: *mut std::ffi::c_void,
                error: *mut *mut std::ffi::c_char,
            ) -> *mut std::ffi::c_void;
            fn sandbox_free_profile(profile: *mut std::ffi::c_void);
            fn sandbox_free_error(error: *mut std::ffi::c_char);
        }
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for network in [false, true] {
            let config = SandboxConfig {
                network,
                profiles: vec!["cargo".into(), "python".into(), "node".into()],
                write_paths: vec![cwd.join("quote\"backslash\\newline\nreturn\rtab\t")],
                ..Default::default()
            };
            let text =
                std::ffi::CString::new(build_profile(&cwd, &config, Some(&cwd)).unwrap()).unwrap();
            let mut error = std::ptr::null_mut();
            // SAFETY: the compiler borrows live C strings; returned objects are
            // released once with their matching Seatbelt deallocators.
            unsafe {
                let profile =
                    sandbox_compile_string(text.as_ptr(), std::ptr::null_mut(), &mut error);
                let message = if error.is_null() {
                    String::new()
                } else {
                    let message = std::ffi::CStr::from_ptr(error)
                        .to_string_lossy()
                        .into_owned();
                    sandbox_free_error(error);
                    message
                };
                if !profile.is_null() {
                    sandbox_free_profile(profile);
                }
                assert!(!profile.is_null(), "SBPL compilation failed: {message}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires a macOS host that permits sandbox-exec (not an already restricted runner)"]
    async fn macos_allows_workspace_and_exact_grant_but_blocks_other_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cwd = root.join("workspace");
        let grant = root.join("cache"); // deliberately does not exist yet
        std::fs::create_dir(&cwd).unwrap();
        let config = SandboxConfig {
            enabled: true,
            write_paths: vec![grant.clone()],
            ..Default::default()
        };
        let mut child = command(&cwd, &config).unwrap();
        child
            .arg("-c")
            .arg(
                r#"
            set -eu
            test "$TMPDIR" = "$PWD/.ilar-cache/tmp"
            printf allowed > "$1/allowed"
            mkdir "$2"
            printf allowed > "$2/allowed"
            printf allowed > /dev/null
            if (printf blocked > "$3/blocked") 2>/dev/null; then exit 41; fi
            if (mkdir "$3/cache-sibling") 2>/dev/null; then exit 42; fi
            if (printf blocked > "$1/escape/blocked") 2>/dev/null; then exit 43; fi
        "#,
            )
            .arg("sandbox-test")
            .arg(&cwd)
            .arg(&grant)
            .arg(&root);
        // A link inside the workspace must not permit writes outside it.
        std::os::unix::fs::symlink(&root, cwd.join("escape")).unwrap();
        let output = child
            .output()
            .await
            .expect("sandbox-exec must start on this host");
        assert!(
            output.status.success(),
            "{:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("allowed")).unwrap(),
            "allowed"
        );
        assert_eq!(
            std::fs::read_to_string(grant.join("allowed")).unwrap(),
            "allowed"
        );
        assert!(!root.join("blocked").exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires macOS sandbox-exec and installed Go, node and mise"]
    async fn macos_go_cgo_and_mise_project_tasks_use_only_local_caches() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("go.mod"),
            "module sandboxprobe\n\ngo 1.24\n",
        )
        .unwrap();
        std::fs::write(workspace.path().join("probe.go"), "package sandboxprobe\n/* int value() { return 42; } */\nimport \"C\"\nfunc Value() int { return int(C.value()) }\n").unwrap();
        std::fs::write(workspace.path().join("probe_test.go"), "package sandboxprobe\nimport \"testing\"\nfunc TestValue(t *testing.T) { if Value() != 42 { t.Fatal(Value()) } }\n").unwrap();
        std::fs::write(workspace.path().join("mise.toml"), "[tools]\ngo = 'latest'\nnode = 'lts'\n[tasks.probe]\nrun = 'go version && node --version'\n").unwrap();
        let config = SandboxConfig {
            enabled: true,
            ..Default::default()
        };
        let output = command(workspace.path(), &config)
            .unwrap()
            .args([
                "-c",
                r#"
            set -eu
            test "$TMPDIR" != "$PWD"
            GOPROXY=off GOSUMDB=off CGO_ENABLED=1 go test ./...
            mise run probe
            mise exec -- go version
            mise exec -- node --version
        "#,
            ])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires a macOS host that permits sandbox-exec and localhost TCP"]
    async fn macos_network_requires_explicit_grant() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for network in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let mut request = [0; 1024];
                    let _ = socket.read(&mut request).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                }
            });
            let config = SandboxConfig {
                enabled: true,
                network,
                ..Default::default()
            };
            let mut child = command(&cwd, &config).unwrap();
            child
                .args([
                    "-c",
                    "exec /usr/bin/curl --silent --show-error --noproxy '*' --max-time 3 \"$1\"",
                    "sandbox-test",
                ])
                .arg(format!("http://{address}/"));
            let output = child
                .output()
                .await
                .expect("sandbox-exec must start on this host");
            server.abort();
            assert_eq!(
                output.status.success(),
                network,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if network {
                assert_eq!(output.stdout, b"ok");
            }
        }
    }
}
