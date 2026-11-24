//! QA harnesses for the hand-off and shutdown regressions. They exercise
//! private modules (`cursor_hide`, `coalesce`) and pin source contracts, so
//! they are unit tests of the library rather than integration tests that
//! compile copies of those files.

mod handoff_old_receiver;
mod handoff_shutdown;
