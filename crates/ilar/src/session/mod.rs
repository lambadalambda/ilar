//! JSONL session store — see meta/issues/session-jsonl.md.

mod event;
mod live;
mod model;
mod replay_index;
mod store;
mod tail;

pub use event::{SessionEvent, SessionMeta, SessionState, new_id};
pub use live::{
    LIVE_SUFFIX, LiveDelta, LiveScratch, SCRATCH_HEARTBEAT, live_path, parse_scratch,
    sweep_live_scratches,
};
pub use model::{ChatMessage, ContentBlock, ImageContent, InputTokenAccounting, Role, Usage};
pub use store::{
    ChildSummary, PendingQuestion, RewindOutcome, Session, SessionHead, SessionId, SessionReader,
    SessionStore, SessionSummary, SessionWriter, transcript_of,
};
pub use tail::{SessionTail, TailUpdate};
