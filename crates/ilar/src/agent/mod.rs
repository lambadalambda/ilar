//! Agent loop — see meta/issues/agent-loop.md.

mod event;
mod turn;

pub use event::{
    LOOP_EVENT_CAPACITY, LoopEvent, LoopEventReceiver, LoopEventSender, loop_event_channel,
};
pub use turn::{
    LoopConfig, TurnOutcome, run_turn, summarize_task_input, summarize_tool_input,
    tool_argument_detail,
};
