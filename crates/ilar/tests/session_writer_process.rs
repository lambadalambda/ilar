use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use ilar::session::{SessionMeta, SessionStore, new_id};

const MODE: &str = "ILAR_LOCK_TEST_MODE";
const ROOT: &str = "ILAR_LOCK_TEST_ROOT";
const ID: &str = "ILAR_LOCK_TEST_ID";
const READY: &str = "ILAR_LOCK_TEST_READY";

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(command: &mut Command) -> std::io::Result<Self> {
        command.spawn().map(|child| Self(Some(child)))
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.as_mut().expect("child already reaped").try_wait()
    }

    fn wait_timeout(&mut self, timeout: Duration) -> std::io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                self.0.take();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "child process timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_wait(&mut self) -> std::io::Result<()> {
        let child = self.0.as_mut().expect("child already reaped");
        child.kill()?;
        child.wait()?;
        self.0.take();
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn helper_command(mode: &str, root: &std::path::Path, id: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "process_lock_helper"])
        .env(MODE, mode)
        .env(ROOT, root)
        .env(ID, id);
    command
}

#[test]
fn process_lock_helper() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let store = SessionStore::new(std::env::var_os(ROOT).unwrap().into());
    let id = std::env::var(ID).unwrap();

    match mode.as_str() {
        "contend" => {
            let error = store
                .acquire_writer(&id)
                .err()
                .expect("another process owns the writer lease");
            assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            assert!(error.to_string().contains("already active"));
        }
        "hold" => {
            let _writer = store.acquire_writer(&id).unwrap();
            std::fs::write(std::env::var_os(READY).unwrap(), b"ready").unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => panic!("unknown lock-test mode: {mode}"),
    }
}

#[test]
fn writer_lease_contends_across_processes_and_releases_after_exit() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().to_path_buf());
    let id = new_id();
    drop(
        store
            .create(SessionMeta {
                session_id: id.clone(),
                parent_id: None,
                agent: "build".into(),
                model: "zai/glm-4.7".into(),
            })
            .unwrap(),
    );

    let writer = store.acquire_writer(&id).unwrap();
    let mut contender = ChildGuard::spawn(&mut helper_command("contend", dir.path(), &id)).unwrap();
    let status = contender.wait_timeout(Duration::from_secs(5)).unwrap();
    assert!(status.success());
    drop(writer);

    let ready = dir.path().join("lock-holder-ready");
    let mut command = helper_command("hold", dir.path(), &id);
    command.env(READY, &ready);
    let mut holder = ChildGuard::spawn(&mut command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            holder.try_wait().unwrap().is_none(),
            "lock holder exited early"
        );
        assert!(Instant::now() < deadline, "lock holder timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    holder.kill_and_wait().unwrap();

    store.acquire_writer(&id).unwrap();
}
