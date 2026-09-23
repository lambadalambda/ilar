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
///
/// `memory` is whether this seat has the person's memory at all. A
/// room's does not, and telling it where the files are is telling it
/// where to go looking: the seat's tools refuse the directory, but a
/// refusal is a worse answer than never having been pointed there.
///
/// `room` is whether several people are in the chat: then each message
/// arrives as `Name: text`, and the model is told so, or it answers
/// "Alice: …" back.
pub fn block(
    home: &Path,
    workspace: &Path,
    memory: bool,
    room: bool,
    now: DateTime<FixedOffset>,
) -> String {
    format!(
        "# Where you are\n\n\
         You are reached over a chat channel, run by ilar-gateway; the message tool is how \
         you answer, and nothing you write outside it reaches a scheduled turn's chat.{room} Your \
         home is {home}: your SOUL.md, skills/{memory} live there, and \
         your sessions work in {workspace}. A script can wake you with \
         `ilar-gateway notify \"text\"` (or `--to <channel:chat>` for a particular chat), \
         which arrives as a message on the last private chat: use it from cron jobs, services \
         and long builds to report back when they finish, instead of waiting on them. \
         Scheduled turns and heartbeats speak only through the message tool; say nothing \
         when there is nothing to say. This conversation opened at {now}, the local time \
         here with its offset, and every message that reaches you afterwards is stamped \
         with the time it arrived in a <now> tag: reckon \"tomorrow at nine\" from the \
         latest stamp, not from this one, and write a cron expression, or a time without \
         an offset, in UTC.",
        home = home.display(),
        room = if room {
            " This chat is a group: several people are in it, each message reaches you as \
             `Name: text` so you know who is talking, and you reach the chat only when \
             someone speaks to you. Answer in your own voice, without a name in front."
        } else {
            ""
        },
        memory = if memory {
            ", memory/ and the daily notes"
        } else {
            " and your agents"
        },
        workspace = workspace.display(),
        now = now.to_rfc3339(),
    )
}

/// The stamp a turn's prompt carries in. The system prompt's "opened
/// at" is true once and ages from there — a seat a week old would
/// reckon "tomorrow at nine" from last Tuesday — and rewriting it per
/// turn would rewrite the cached prefix with it, at the price of the
/// whole conversation's cache. This rides at the end of the
/// conversation instead, where nothing is cached yet: one short line
/// in front of what was actually said.
pub fn stamped(prompt: &str, now: DateTime<FixedOffset>) -> String {
    format!("<now>{}</now>\n\n{prompt}", now.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn knows when it is, however old the session is: the stamp
    /// rides with the message rather than in the prompt above it.
    #[test]
    fn every_turn_carries_the_time_it_arrived() {
        let opened = DateTime::parse_from_rfc3339("2026-09-09T14:03:11+02:00").unwrap();
        let week_later = DateTime::parse_from_rfc3339("2026-09-16T08:30:00+02:00").unwrap();
        let prompt = stamped("what is on for tomorrow?", week_later);
        assert_eq!(
            prompt,
            "<now>2026-09-16T08:30:00+02:00</now>\n\nwhat is on for tomorrow?"
        );
        // And the system prompt sends the model to it rather than to
        // its own stamp, which is only ever the session's first moment.
        let block = block(
            Path::new("/state/gateway"),
            Path::new("/state/gateway/workspace"),
            true,
            false,
            opened,
        );
        assert!(block.contains("<now> tag"), "{block}");
        assert!(block.contains("latest stamp"), "{block}");
    }

    #[test]
    fn the_block_says_where_and_when_the_session_opened() {
        let now = DateTime::parse_from_rfc3339("2026-09-15T14:03:11+02:00").unwrap();
        let text = block(
            Path::new("/state/gateway"),
            Path::new("/state/gateway/workspace"),
            true,
            false,
            now,
        );
        assert!(text.contains("/state/gateway/workspace"), "{text}");
        // The offset is the point: a model with no bash cannot run
        // `date`, and a cron expression it writes is read as UTC.
        assert!(text.contains("2026-09-15T14:03:11+02:00"), "{text}");
        assert!(text.contains("in UTC"), "{text}");
    }

    /// A seat without the memory is not told where the memory is: its
    /// tools would refuse the directory, and a refusal is a worse
    /// answer than never having been sent there.
    #[test]
    fn a_seat_without_memory_is_not_told_where_it_is() {
        let now = DateTime::parse_from_rfc3339("2026-09-15T14:03:11+02:00").unwrap();
        let room = block(
            Path::new("/state/gateway"),
            Path::new("/state/gateway/workspace"),
            false,
            true,
            now,
        );
        assert!(!room.contains("memory/"), "{room}");
        assert!(room.contains("`Name: text`"), "{room}");
        assert!(room.contains("without a name in front"), "{room}");
        assert!(room.contains("/state/gateway"), "{room}");

        let private = block(
            Path::new("/state/gateway"),
            Path::new("/state/gateway/workspace"),
            true,
            false,
            now,
        );
        assert!(private.contains("memory/"), "{private}");
        assert!(!private.contains("group"), "{private}");
    }
}
