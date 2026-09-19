//! The parts of the subc module that are not the frame loop.
//!
//! EXISTS SO DIAGNOSTIC EXAMPLES CAN USE THE REAL VAULT CLIENT rather than a
//! second implementation of the same protocol. Without a library target, an
//! example wanting a vault-served credential has three options and two of them
//! are wrong:
//!
//! * speak `credential.get` itself -- a second reader of one protocol, which
//!   drifts from this one and then asserts something subtly different. That
//!   defect was live in `vault-lanes.rs` twice (60ba7ee, 8bcc382) and both times
//!   the checker disagreed with the module about what it was checking.
//! * add the call to a provider and deploy it to read the answer -- diagnostic
//!   code in the serving binary, and a restart that discards the in-memory drop
//!   ring and every process-lifetime counter.
//! * use the client the module uses. This.
//!
//! `main.rs` stays the binary and keeps its frame loop; only the two modules an
//! outside caller could legitimately need are published here.
//!
//! One copy, not two: `main.rs` imports these from the library rather than
//! declaring `mod` again, which would compile the same source twice and let the
//! binary and an example disagree while both look correct.

pub mod ids;
pub mod vault_client;
