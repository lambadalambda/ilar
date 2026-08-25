//! Crash-durable replacement bound to one Unix directory descriptor.
//! Non-Unix targets fail closed until equivalent handle-relative APIs land.

#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
use std::io::{Error, ErrorKind};
use std::path::Path;

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Preserve,
    Force(u32),
}

pub(crate) fn replace(path: &Path, content: &[u8], mode: Mode) -> std::io::Result<()> {
    replace_with(path, content, mode, &NoHooks)
}

pub(crate) fn replace_cancellable(
    path: &Path,
    content: &[u8],
    mode: Mode,
    cancel: &tokio_util::sync::CancellationToken,
) -> std::io::Result<()> {
    replace_with(path, content, mode, &CancelHooks(cancel))
}

trait Hooks {
    fn before_write(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn before_rename(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn after_rename(&self) -> std::io::Result<()> {
        Ok(())
    }
}

struct NoHooks;
impl Hooks for NoHooks {}

struct CancelHooks<'a>(&'a tokio_util::sync::CancellationToken);

impl Hooks for CancelHooks<'_> {
    fn before_write(&self) -> std::io::Result<()> {
        self.check()
    }

    fn before_rename(&self) -> std::io::Result<()> {
        self.check()
    }
}

impl CancelHooks<'_> {
    fn check(&self) -> std::io::Result<()> {
        if self.0.is_cancelled() {
            Err(Error::new(ErrorKind::Interrupted, "write cancelled"))
        } else {
            Ok(())
        }
    }
}

fn replace_with(path: &Path, content: &[u8], mode: Mode, hooks: &dyn Hooks) -> std::io::Result<()> {
    #[cfg(unix)]
    return replace_unix(path, content, mode, hooks);
    #[cfg(not(unix))]
    return replace_portable(path, content, mode, hooks);
}

#[cfg(unix)]
fn replace_unix(path: &Path, content: &[u8], mode: Mode, hooks: &dyn Hooks) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = parent_of(path);
    let destination_name = path
        .file_name()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "destination has no filename"))?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let directory = options.open(parent)?;
    let identity = directory.metadata()?;
    let destination = metadata_at(directory.as_raw_fd(), destination_name)?;
    refuse_symlink(path, destination.as_ref())?;
    let final_mode = match mode {
        Mode::Preserve => Some(destination.as_ref().map_or_else(
            || 0o666 & !process_umask(),
            |metadata| metadata.st_mode as u32 & 0o7777,
        )),
        Mode::Force(mode) => Some(mode),
    };
    let (temp_name, mut temp) = create_temp_at(directory.as_raw_fd(), destination_name)?;
    let temp_c = c_string(std::ffi::OsStr::new(&temp_name))?;
    let destination_c = c_string(destination_name)?;

    let pre_publication = (|| {
        hooks.before_write()?;
        temp.write_all(content)?;
        if let Some(mode) = final_mode {
            temp.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        temp.sync_all()?;
        hooks.before_rename()?;
        ensure_visible_parent(parent, &identity)?;
        refuse_symlink(
            path,
            metadata_at(directory.as_raw_fd(), destination_name)?.as_ref(),
        )?;
        let result = unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temp_c.as_ptr(),
                directory.as_raw_fd(),
                destination_c.as_ptr(),
            )
        };
        if result != 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    })();

    if let Err(primary) = pre_publication {
        let removed = unsafe { libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0) };
        let cleanup = (removed != 0).then(Error::last_os_error);
        return Err(combine_cleanup(primary, cleanup, &temp_name));
    }

    let parent_error = hooks
        .after_rename()
        .and_then(|()| ensure_visible_parent(parent, &identity))
        .err();
    let sync_error = directory.sync_all().err();
    match (parent_error, sync_error) {
        (None, None) => Ok(()),
        (Some(parent), None) => Err(Error::new(
            parent.kind(),
            format!("replacement published but parent changed: {parent}"),
        )),
        (None, Some(sync)) => Err(Error::new(
            sync.kind(),
            format!("replacement published but directory sync failed: {sync}"),
        )),
        (Some(parent), Some(sync)) => Err(Error::new(
            parent.kind(),
            format!(
                "replacement published but parent changed: {parent}; directory sync also failed: {sync}"
            ),
        )),
    }
}

/// Creation permissions for a destination that does not exist yet.
///
/// `libc::umask` only reports the mask by setting it, so the read is done once
/// and cached: the window where the process mask is temporarily `0` exists on
/// first use only.
#[cfg(unix)]
fn process_umask() -> u32 {
    static UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UMASK.get_or_init(|| {
        let mask = unsafe { libc::umask(0) };
        unsafe { libc::umask(mask) };
        u32::from(mask)
    })
}

#[cfg(unix)]
fn create_temp_at(
    directory: std::os::fd::RawFd,
    _destination_name: &std::ffi::OsStr,
) -> std::io::Result<(String, File)> {
    use std::os::fd::FromRawFd;
    for _ in 0..16 {
        let name = format!(".ilar-tmp-{}", uuid::Uuid::new_v4());
        let name_c = c_string(std::ffi::OsStr::new(&name))?;
        let fd = unsafe {
            libc::openat(
                directory,
                name_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            return Ok((name, unsafe { File::from_raw_fd(fd) }));
        }
        let error = Error::last_os_error();
        if error.kind() != ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Err(Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate unique replacement file",
    ))
}

#[cfg(unix)]
fn metadata_at(
    directory: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
) -> std::io::Result<Option<libc::stat>> {
    let name = c_string(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(unsafe { metadata.assume_init() }))
    } else {
        let error = Error::last_os_error();
        if error.kind() == ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
fn refuse_symlink(path: &Path, metadata: Option<&libc::stat>) -> std::io::Result<()> {
    if metadata.is_some_and(|metadata| metadata.st_mode & libc::S_IFMT == libc::S_IFLNK) {
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to replace symlink {}", path.display()),
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn ensure_visible_parent(parent: &Path, expected: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let actual = std::fs::symlink_metadata(parent)?;
    if actual.file_type().is_symlink() {
        return Err(Error::other(format!(
            "destination parent became a symlink: {}",
            parent.display()
        )));
    }
    if actual.dev() == expected.dev() && actual.ino() == expected.ino() {
        Ok(())
    } else {
        Err(Error::other(format!(
            "destination parent changed during replacement: {}",
            parent.display()
        )))
    }
}

#[cfg(unix)]
fn c_string(value: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(value.as_bytes())
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(not(unix))]
fn replace_portable(
    _path: &Path,
    _content: &[u8],
    _mode: Mode,
    _hooks: &dyn Hooks,
) -> std::io::Result<()> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "secure atomic replacement is currently supported only on Unix",
    ))
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn combine_cleanup(primary: Error, cleanup: Option<Error>, temp: &str) -> Error {
    match cleanup {
        None => primary,
        Some(cleanup) if cleanup.kind() == ErrorKind::NotFound => primary,
        Some(cleanup) => Error::new(
            primary.kind(),
            format!("{primary}; additionally failed to remove {temp}: {cleanup}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    enum Failure {
        Write,
        Rename,
    }

    impl Hooks for Failure {
        fn before_write(&self) -> std::io::Result<()> {
            match self {
                Self::Write => Err(Error::other("injected write failure")),
                Self::Rename => Ok(()),
            }
        }
        fn before_rename(&self) -> std::io::Result<()> {
            match self {
                Self::Rename => Err(Error::other("injected rename failure")),
                Self::Write => Ok(()),
            }
        }
    }

    fn temp_entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(".ilar-tmp-"))
            .collect()
    }

    #[test]
    fn injected_failures_preserve_original_and_clean_temp() {
        for failure in [Failure::Write, Failure::Rename] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("target");
            std::fs::write(&path, b"original").unwrap();
            assert!(replace_with(&path, b"replacement", Mode::Preserve, &failure).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"original");
            assert!(temp_entries(dir.path()).is_empty());
        }
    }

    #[test]
    fn replacement_publishes_complete_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"old").unwrap();
        replace(&path, b"complete replacement", Mode::Preserve).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"complete replacement");
    }

    #[test]
    fn cancelled_replacement_preserves_original_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"original").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let error = replace_cancellable(&path, b"replacement", Mode::Preserve, &cancel)
            .expect_err("cancelled replacement must fail");

        assert_eq!(error.kind(), ErrorKind::Interrupted);
        assert_eq!(std::fs::read(path).unwrap(), b"original");
        assert!(temp_entries(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn post_publication_error_does_not_enter_temp_cleanup() {
        struct FailAfterRename;
        impl Hooks for FailAfterRename {
            fn after_rename(&self) -> std::io::Result<()> {
                Err(Error::other("injected post-rename failure"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"old").unwrap();

        let error = replace_with(&path, b"published", Mode::Preserve, &FailAfterRename)
            .expect_err("post-publication state must be reported");
        assert!(error.to_string().contains("replacement published"));
        assert_eq!(std::fs::read(path).unwrap(), b"published");
        assert!(temp_entries(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_preserved_or_forced() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        replace(&path, b"preserved", Mode::Preserve).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
        replace(&path, b"secret", Mode::Force(0o600)).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn new_file_honors_umask_while_overwrite_keeps_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh");
        replace(&fresh, b"created", Mode::Preserve).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().mode() & 0o7777,
            0o666 & !process_umask()
        );

        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o604)).unwrap();
        replace(&fresh, b"overwritten", Mode::Preserve).unwrap();
        assert_eq!(std::fs::metadata(&fresh).unwrap().mode() & 0o7777, 0o604);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_destination_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, b"safe").unwrap();
        symlink(&target, &link).unwrap();
        assert!(replace(&link, b"unsafe", Mode::Preserve).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"safe");
    }

    #[cfg(unix)]
    #[test]
    fn parent_swap_is_rejected_and_temp_is_cleaned_via_directory_handle() {
        struct Swap {
            parent: PathBuf,
            moved: PathBuf,
        }
        impl Hooks for Swap {
            fn before_rename(&self) -> std::io::Result<()> {
                std::fs::rename(&self.parent, &self.moved)?;
                std::fs::create_dir(&self.parent)
            }
        }
        let outer = tempfile::tempdir().unwrap();
        let parent = outer.path().join("parent");
        let moved = outer.path().join("moved");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("target");
        std::fs::write(&path, b"original").unwrap();
        let swap = Swap {
            parent: parent.clone(),
            moved: moved.clone(),
        };
        assert!(replace_with(&path, b"replacement", Mode::Preserve, &swap).is_err());
        assert_eq!(std::fs::read(moved.join("target")).unwrap(), b"original");
        assert!(temp_entries(&moved).is_empty());
    }
}
