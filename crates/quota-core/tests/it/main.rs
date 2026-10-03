//! Every quota-core integration test, compiled into ONE test binary.
//!
//! Each file directly under `tests/` is its own executable, and on macOS every
//! freshly built executable pays a signature scan on its first run, serialised
//! host-wide. Folding the suites into modules of one binary pays that once.
//!
//! All of these are live provider proofs and stay `#[ignore]`: they read real
//! sessions on the machine running them. None mutates the process environment
//! or cwd, so they share a process safely. Run one with a name filter, e.g.
//! `cargo test -p quota-core --test it gemini_live:: -- --ignored --nocapture`.

mod antigravity_live;
mod gemini_live;
mod grok_live;
mod jetbrains_live;
