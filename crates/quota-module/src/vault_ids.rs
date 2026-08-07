//! The credential vault's identity on the daemon, in one place.
//!
//! This lives in its own file so the integration tests can include it by path
//! rather than restating it. The module is a binary crate, so a test cannot
//! `use` anything from it — and when the id was written out twice, a rename of
//! the vault left the client dialling the new name while the test stub still
//! registered under the old one. Nothing failed to compile; the test simply
//! stopped exercising the vault and reported that two accounts never arrived.
//!
//! One definition means that divergence cannot recur.
//!
//! It also means the integration test can no longer tell whether this value is
//! *correct*, only that both sides agree: change the string here and the stub
//! registers under the new name too, so the test passes. That check has to come
//! from outside the process, against the real daemon — `cargo run -p
//! quota-module --example vault-lanes` requires every configured credential to
//! be serving usage from a vault source, which is producible only by a live
//! credential fetch through whatever id this actually names.

/// The daemon module id of the credential vault.
///
/// This is a TARGET reference: the id dialled to reach the vault, not a name
/// anyone checks against us. So it does not gate a rename on that side — it
/// breaks the moment one lands, and every vault-served provider loses its
/// credential while local-credential providers carry on unaffected.
///
/// The failure is quiet by construction. A wrong id answers `unknown_module`,
/// which is classified transient (a restarting module answers identically), so
/// the refresher retries on its backoff forever and never reaches a verdict.
pub const CREDENTIALS_MODULE_ID: &str = "claustrum";
