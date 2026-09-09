//! The commit the binary was built from, for the start announcement:
//! `ILAR_GATEWAY_COMMIT`, empty when git is not there to ask.

use std::process::Command;

fn git_path(name: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
}

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=ILAR_GATEWAY_COMMIT={commit}");
    // Where HEAD actually lives: in a worktree `.git` is a file.
    for name in ["HEAD", "refs/heads"] {
        if let Some(path) = git_path(name) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
