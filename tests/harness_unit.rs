//! The one binary that compiles the harness's own unit tests — issue #806.
//!
//! It exists because no `#[cfg]` available here distinguishes one test binary
//! from another — `cfg(test)` is true in every one of them — and the module
//! tree does. `tests/common/harness_unit_tests.rs` is named here and nowhere
//! else, so the 108 tests in it are compiled, and run, once: not once in each
//! binary that writes `mod common;` (65 of them under lane 1 when this was
//! measured). The head of that file carries the measurement and the rule for
//! what belongs in it.
//!
//! Nothing else goes in this file. It links the harness only so those tests
//! can reach it; it starts no deck and spawns no agent, which is exactly why
//! the tests it hosts are indifferent to being here.

mod common;

#[path = "common/harness_unit_tests.rs"]
mod harness_unit_tests;
