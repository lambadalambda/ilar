//! JSONL session store — see meta/issues/session-jsonl.md.

mod event;
mod live;
mod model;
mod replay_index;
mod store;
mod summary_cache;
mod tail;

pub use event::{SessionEvent, SessionMeta, SessionState, new_id};
pub use live::{
    LIVE_SUFFIX, LiveDelta, LiveScratch, SCRATCH_HEARTBEAT, live_path, parse_scratch,
    sweep_live_scratches,
};
pub use model::{
    ChatMessage, ContentBlock, DiagnosticKind, ImageContent, InputTokenAccounting, Role, Usage,
};
pub use store::{
    ChildSummary, PendingQuestion, RewindOutcome, Session, SessionHead, SessionId, SessionReader,
    SessionStore, SessionSummary, SessionWriter, compaction_cut, sweep_stale_locks, transcript_of,
};
pub use tail::{SessionTail, TailUpdate};
