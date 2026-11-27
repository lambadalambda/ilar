//! A terminal stand-in for the phone, through n0's relay:
//! `cargo run -p ilar-remote --example pair -- <link>`.
//!
//! Prints one line per message from the session. Each stdin line is
//! sent: `/cancel`, `/grant <secret> once|session|always|deny`,
//! anything else as a prompt. Ctrl-D hangs up.

use ilar_remote::wire::{FromDevice, GrantAnswer, ToDevice};
use ilar_remote::{ALPN, Ticket, client};
use iroh::{Endpoint, endpoint::presets};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let link = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: pair <link or ticket>"))?;
    let ticket = Ticket::decode(&link)?;
    let endpoint = Endpoint::bind(presets::N0).await?;
    let started = std::time::Instant::now();
    let conn = endpoint.connect(ticket.addr(), ALPN).await?;
    let mut session = client::hello(&conn, Some(&ticket.secret)).await?;
    println!("paired as {} in {:?}", endpoint.id(), started.elapsed());
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            message = session.next() => match message? {
                Some(message) => println!("< {}", summary(&message)),
                None => break,
            },
            line = stdin.next_line() => match line? {
                Some(line) => match command(&line) {
                    Some(command) => session.send(&command).await?,
                    None => println!("? /grant <secret> once|session|always|deny"),
                },
                None => break,
            },
        }
    }
    conn.close(0u32.into(), b"bye");
    endpoint.close().await;
    Ok(())
}

/// `None` for a line that is not a command: it is reported, not sent.
fn command(line: &str) -> Option<FromDevice> {
    let answer = |word: &str| match word {
        "once" => Some(Some(GrantAnswer::Once)),
        "session" => Some(Some(GrantAnswer::Session)),
        "always" => Some(Some(GrantAnswer::Always)),
        "deny" => Some(None),
        _ => None,
    };
    if line.trim() == "/cancel" {
        return Some(FromDevice::Cancel);
    }
    match line.trim().strip_prefix("/grant ") {
        // `/grant <secret> <once|session|always|deny>`
        Some(rest) => {
            let (secret, word) = rest.trim().split_once(' ')?;
            Some(FromDevice::Grant {
                secret: secret.into(),
                answer: answer(word.trim())?,
            })
        }
        None => Some(FromDevice::Prompt {
            text: line.into(),
            images: Vec::new(),
        }),
    }
}

fn summary(message: &ToDevice) -> String {
    let text = match message {
        ToDevice::Snapshot {
            page,
            live,
            busy,
            prompt,
        } => format!(
            "snapshot: {} events at line {}, {} live, busy {busy}, prompt {}",
            page["events"].as_array().map_or(0, Vec::len),
            page["line"],
            live.len(),
            prompt.is_some()
        ),
        other => serde_json::to_string(other).expect("serializes"),
    };
    text.chars().take(160).collect()
}
