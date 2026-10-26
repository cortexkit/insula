//! Exercise `credential.list_scoped` against the running vault and report what
//! came back.
//!
//! The module enumerates credentials through scoped grants. This diagnostic
//! checks the listing shape without fetching credential payloads. It needs an
//! authorized caller principal; a standalone direct caller may be refused.
//!
//! PRINTS NO SECRET. The reply carries no credential material: `list_scoped`
//! enumerates identities and states, and the bearer is fetched separately by
//! `credential.get_scoped`. Identity fields are echoed because they are the
//! thing being checked, and they already appear on our own wire.
//!
//! Exit 0 clean, 1 findings, 2 could not check.

use quota_module::vault_client::VaultClient;

#[path = "support/connection.rs"]
mod connection;

#[tokio::main]
async fn main() {
    let (path, _) = connection::connection_and_args();

    let client = VaultClient::new(path);
    let listing = match client.list_scoped_report().await {
        Ok(listing) => listing,
        Err(error) => {
            eprintln!("  credential.list_scoped refused: {error:?}");
            eprintln!("  check the caller's scoped grants and daemon connection");
            eprintln!("  exit 2: no credential inventory was observed");
            std::process::exit(2);
        }
    };

    println!("  credential.list_scoped");
    println!("    view digest: {} chars", listing.view_len);
    println!("    grants: {}", listing.grants);
    println!("    credentials: {}", listing.credentials.len());

    // DENOMINATORS BEFORE ROWS. A clean run has to name the population it
    // examined, or a zero reads as "all good" when it means "saw nothing".
    let with_identity = listing
        .credentials
        .iter()
        .filter(|row| row.account_id.is_some())
        .count();
    let non_active = listing
        .credentials
        .iter()
        .filter(|row| row.state != "active")
        .count();
    println!("    of those: {with_identity} carry an account id, {non_active} are not active");

    for row in &listing.credentials {
        println!(
            "      {:38} state={:14} v{:<6} acct={} email={}",
            row.id,
            row.state,
            row.record_version,
            row.account_id.as_deref().unwrap_or("-"),
            row.email.as_deref().unwrap_or("-"),
        );
    }

    let mut findings: Vec<String> = Vec::new();

    // THE FIELD THE CUTOVER RESTS ON. The slot key becomes
    // `(provider, credential_id, record_version)`, so a row without a usable
    // version cannot be keyed and the whole recovery-latency improvement is
    // silently lost for it.
    if listing
        .credentials
        .iter()
        .any(|row| row.record_version == 0)
    {
        findings.push("a row carries record_version 0, which cannot key a slot".to_string());
    }

    // An empty list with grants held is a legitimate state and NOT a finding:
    // "authorized and genuinely empty". An empty list with no grants is the
    // deauthorized-or-never-authorized case, which the wire deliberately cannot
    // tell apart -- so it is reported without being named.
    if listing.credentials.is_empty() {
        if listing.grants == 0 {
            println!("    no grants and no rows: this consumer is not authorized here");
            println!("    (the wire cannot distinguish that from never having been)");
        } else {
            println!("    grants held, no visible rows: authorized and genuinely empty");
        }
    }

    if findings.is_empty() {
        println!("  findings: none");
        return;
    }
    println!("  findings: {}", findings.len());
    for finding in &findings {
        println!("    {finding}");
    }
    std::process::exit(1);
}
