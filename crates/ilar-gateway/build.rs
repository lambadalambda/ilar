//! The commit the binary was built from, for the start announcement:
//! `ILAR_GATEWAY_COMMIT`, empty when git is not there to ask.

use std::process::Command;

fn main() {
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=ILAR_GATEWAY_COMMIT={commit}");
    for path in ["../../.git/HEAD", "../../.git/refs/heads"] {
        println!("cargo:rerun-if-changed={path}");
    }
}
