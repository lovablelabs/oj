// Go internal/project/background (+ the timer thread that stands in for Go's `time.AfterFunc`).

pub mod queue;
mod timer;

pub use queue::*;
pub use timer::*;
