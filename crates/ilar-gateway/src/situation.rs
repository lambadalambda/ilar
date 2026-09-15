//! The block that tells a gateway session where it is: reached over a
//! chat, with a home, wakeable from a script. The base prompt is about
//! tools and the SOUL.md about character; neither says this.

use std::path::Path;

use chrono::{DateTime, FixedOffset};

/// Appended to every gateway session's system prompt. `now` is when
/// the session opens, in the machine's own zone: a model under a tool
/// policy that withholds `bash` has no other way to know the date,
/// the time or the person's offset, and a schedule it writes is in
/// UTC.
pub fn block(home: &Path, workspace: &Path, now: DateTime<FixedOffset>) -> String {
    format!(
        "# Where you are\n\n\
         You are reached over a chat channel, run by ilar-gateway; the message tool is how \
         you answer, and nothing you write outside it reaches a scheduled turn's chat. Your \
         home is {home}: your SOUL.md, skills/, memory/ and the daily notes live there, and \
         your sessions work in {workspace}. A script can wake you with \
         `ilar-gateway notify \"text\"` (or `--to <channel:chat>` for a particular chat), \
         which arrives as a message on the last active chat: use it from cron jobs, services \
         and long builds to report back when they finish, instead of waiting on them. \
         Scheduled turns and heartbeats speak only through the message tool; say nothing \
         when there is nothing to say. This conversation opened at {now}, the local time \
         here with its offset: reckon \"tomorrow at nine\" from that, and write a cron \
         expression, or a time without an offset, in UTC. The stamp ages as the \
         conversation goes on.",
        home = home.display(),
        workspace = workspace.display(),
        now = now.to_rfc3339(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_says_where_and_when_the_session_opened() {
        let now = DateTime::parse_from_rfc3339("2026-09-15T14:03:11+02:00").unwrap();
        let text = block(
            Path::new("/state/gateway"),
            Path::new("/state/gateway/workspace"),
            now,
        );
        assert!(text.contains("/state/gateway/workspace"), "{text}");
        // The offset is the point: a model with no bash cannot run
        // `date`, and a cron expression it writes is read as UTC.
        assert!(text.contains("2026-09-15T14:03:11+02:00"), "{text}");
        assert!(text.contains("in UTC"), "{text}");
    }
}
