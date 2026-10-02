#![forbid(unsafe_code)]

//! Every quota-module integration test, compiled into ONE test binary.
//!
//! Each file directly under `tests/` is its own executable, and on macOS every
//! freshly built executable pays a signature scan on its first run, serialised
//! host-wide. Folding the suites into modules of one binary pays that once.
//!
//! Sharing a process is safe here because neither suite touches process-wide
//! state: `common::isolate_env` clears and sets the environment of the CHILD
//! `Command` it is given, never this process's, and nothing sets the cwd or a
//! global subscriber. The one re-exec (`skeleton_e2e`'s environment probe runs
//! this binary again filtered to a single test) names that test by its full
//! module path, so it still selects exactly one test.
//!
//! Select a suite with a name filter:
//! `cargo test -p quota-module --test it skeleton_e2e::` or
//! `cargo test -p quota-module --test it -- --ignored --nocapture real_daemon_e2e::`.

// The shared wire driver stays at `tests/common/mod.rs` rather than moving in
// here: the `vault-lanes` and `deployed-sanity` examples include that exact
// path too. Cargo builds no target from it, since it is not `tests/*.rs`.
#[path = "../common/mod.rs"]
mod common;
mod real_daemon_e2e;
mod skeleton_e2e;
