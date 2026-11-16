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
//! and `TEMP` are set to the workspace itself, so temporary files stay within its
//! existing write grant. This is not a separate private temporary directory and
//! temporary-file cleanup is the child's responsibility.
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
        for key in ["TMPDIR", "TMP", "TEMP"] {
            command.env(key, &workspace);
        }
        Ok(command)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cwd, config, executable, grant_executable);
        anyhow::bail!("experimental process sandbox is supported only on macOS")
    }
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

#[cfg(any(target_os = "macos", test))]
fn build_profile(
    cwd: &Path,
    config: &SandboxConfig,
    home: Option<&Path>,
) -> anyhow::Result<String> {
    let mut grants = vec![
        format!("(subpath {})", sbpl_path(cwd)?),
        "(literal \"/dev/null\")".into(),
    ];
    let mut reads = grants.clone();
    for path in &config.write_paths {
        grants.push(format!("(subpath {})", sbpl_path(&resolve_path(path)?)?));
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
        for (filter, paths) in [("subpath", subtrees), ("literal", files)] {
            for path in paths {
                let expected = home.join(path);
                let resolved = resolve_path(&expected)?;
                ensure!(
                    resolved == expected,
                    "sandbox cache path redirects through a symlink: {}; use an explicit write_paths grant instead",
                    expected.display()
                );
                let grant = format!("({filter} {})", sbpl_path(&resolved)?);
                reads.push(grant.clone());
                grants.push(grant);
            }
        }
    }
    grants.sort();
    grants.dedup();
    for path in &config.read_paths {
        reads.push(format!("(subpath {})", sbpl_path(&resolve_path(path)?)?));
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
        "/Library/Apple",
        "/Library/Developer",
        "/Applications/Xcode.app",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/opt/homebrew/lib",
        "/opt/homebrew/opt",
        "/opt/homebrew/Cellar",
        "/opt/homebrew/share",
        "/private/var/db/dyld",
        "/private/var/db/timezone",
        "/private/etc/ssl",
    ] {
        reads.push(format!("(subpath {})", sbpl_string(path)?));
    }
    // dyld probes the root directory on current macOS. Literal, not subpath:
    // root enumeration must not grant access to every file below it.
    reads.push("(literal \"/\")".into());
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
        reads.push(format!("(literal {})", sbpl_string(path)?));
    }
    if let Some(home) = home {
        let home = resolve_path(home)?;
        for path in [
            ".cargo/bin",
            ".rustup/toolchains",
            ".rustup/settings.toml",
            ".local/share/mise/installs",
        ] {
            let expected = home.join(path);
            // Optional defaults must not make unrelated commands fail. Skip
            // redirected/unavailable roots; an explicit grant can authorize
            // their actual target without granting a broader default.
            if let Ok(resolved) = resolve_path(&expected)
                && resolved == expected
            {
                reads.push(format!("(subpath {})", sbpl_path(&resolved)?));
            }
        }
    }
    reads.sort();
    reads.dedup();
    let network = if config.network {
        ""
    } else {
        "(deny network*)\n"
    };
    Ok(format!(
        "(version 1)\n(allow default)\n(deny file-read*)\n(allow file-read-metadata)\n(allow file-read*\n  {})\n(deny file-write*)\n(allow file-write*\n  {})\n{network}",
        reads.join("\n  "),
        grants.join("\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!profile.contains(".local/bin"));
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
                    .any(|(k, v)| k == key && v == Some(cwd.as_os_str()))
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
            test "$TMPDIR" = "$PWD"
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
