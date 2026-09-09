//! The block that tells a gateway session where it is: reached over a
//! chat, with a home, wakeable from a script. The base prompt is about
//! tools and the SOUL.md about character; neither says this.

use std::path::Path;

/// Appended to every gateway session's system prompt.
pub fn block(home: &Path, workspace: &Path) -> String {
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
         when there is nothing to say.",
        home = home.display(),
        workspace = workspace.display(),
    )
}
