//! Exercise `credential.list_scoped` against the running vault and report what
//! came back.
//!
//! WHY THIS EXISTS AS A TOOL RATHER THAN A ONE-OFF. The scoped-grant migration
//! retires `~/.config/cortexkit/ck-quota/vault-handles.json`: the module will
//! enumerate credentials from this call instead, so `ck auth login` alone makes
//! an account appear in `ck quota`. That hand-maintained map has failed three
//! ways on this host in a single day -- an active credential never mapped (an
//! invisible account), a mapped credential later deleted from the vault (a dead
//! row that ALSO suppressed `completeProviders` for four healthy siblings), and
//! a handle minted by an operator that no consumer claimed.
//!
//! THE OP HAS NEVER SERVED A REAL CONSUMER. Its owner says so plainly: its
//! correctness today is warranted by tests written against the behaviour their
//! author imagined. So the first call from here is a MEASUREMENT, and the boring
//! outcome is the one worth reporting -- a consumer confirming the specified
//! shape is the single piece of evidence a producer's own tests structurally
//! cannot produce.
//!
//! PRINTS NO SECRET. The reply carries no credential material: `list_scoped`
//! enumerates identities and states, and the bearer is fetched separately by
//! `credential.get_scoped`. Identity fields are echoed because they are the
//! thing being checked, and they already appear on our own wire.
//!
//! Exit 0 clean, 1 findings, 2 could not check.

use quota_core::credential_source::{CredentialSource, VaultCapability};
use quota_module::vault_client::VaultClient;

/// Any capability from the module's own handle map, purely to address the control
/// op with. Never printed, and the op it drives (`credential.status`) returns no
/// secret -- it reports readiness and a record version.
fn control_handle() -> Option<VaultCapability> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home).join(".config/cortexkit/ck-quota/vault-handles.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let handles = parsed.get("handles")?.as_object()?;
    handles
        .values()
        .find_map(|value| value.as_str())
        .map(VaultCapability::new)
}

fn connection_file() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SUBC_CONNECTION_FILE") {
        return std::path::PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".local/share/cortexkit/run/subc-connection.json")
}

#[tokio::main]
async fn main() {
    let path = connection_file();
    if !path.exists() {
        eprintln!("no daemon connection file at {}", path.display());
        eprintln!("this dials the live vault through the daemon: it must be running");
        std::process::exit(2);
    }

    let client = VaultClient::new(path);
    let listing = match client.list_scoped_report().await {
        Ok(listing) => listing,
        Err(error) => {
            // A REFUSAL IS AMBIGUOUS UNTIL A CONTROL SAYS OTHERWISE, and this one
            // has two very different readings: the vault has nothing for us, or
            // this PROCESS may not ask. Reporting the first without checking is
            // how a correct refusal gets filed as a defect -- which the vault's
            // own maintainer nearly did with their probe, for this exact reason.
            //
            // THE CONTROL: a handle-addressed op from this same process, over the
            // same connection. Scoped ops are authorised by the caller's bus
            // principal, and a standalone example binds as a DIRECT caller rather
            // than as the supervised module, so scoped ops refuse it while handle
            // ops do not. If the control succeeds, the discriminator is the OP
            // CLASS and not the connection, the credentials, or the daemon.
            eprintln!("  credential.list_scoped refused: {error:?}");
            let control = control_handle();
            match control {
                None => {
                    eprintln!("  control: SKIPPED, no handle available to address one with");
                    eprintln!("  exit 2: the refusal is unexplained, which is itself the finding");
                }
                Some(capability) => match client.status(&capability).await {
                    Ok(_) => {
                        eprintln!(
                            "  control: a handle-addressed op from this SAME process SUCCEEDED"
                        );
                        eprintln!(
                            "  so the refusal is about this caller's principal, not the vault:"
                        );
                        eprintln!(
                            "  a standalone probe is a Direct caller, and scoped ops admit the"
                        );
                        eprintln!(
                            "  supervised module by its bus principal or an ENROLLED consumer"
                        );
                        eprintln!(
                            "  holding a token. Neither is a plain dial, so this op cannot be"
                        );
                        eprintln!(
                            "  measured by running this file as-is: the first real call has to"
                        );
                        eprintln!("  come from inside the deployed binary.");
                    }
                    Err(control_error) => {
                        eprintln!("  control: the handle op ALSO refused ({control_error:?})");
                        eprintln!("  so this is the connection or the vault, not the op class");
                    }
                },
            }
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
