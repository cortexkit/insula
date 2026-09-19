//! Does grok's `GetRemainingResets` answer a bearer, and what shape is it?
//!
//! CodexBar v0.61.0 added this endpoint: grok's equivalent of the banked reset
//! credits we publish as `savedResets` for codex. Recorded UNVERIFIED in
//! `docs/provider-matrix.md` rather than declined, because the first probe could
//! not establish either answer.
//!
//! WHY THE FIRST PROBE FAILED, and why this is an example rather than a shell
//! command. It used the local `xai` token from the opencode auth store and got
//! HTTP 200 with zero bytes. The control -- the same token against the endpoint
//! we DO serve -- returned zero bytes too, so the probe had measured an expired
//! token rather than the endpoint. Grok is served from the vault lane here.
//!
//! Reaching a vault credential from a shell is not possible by design: `ck auth`
//! has 27 verbs and none reads a secret, because secrets go to authorized modules
//! over subc. Minting a capability handle to work around that is what produced an
//! orphaned handle earlier -- the mint succeeded, the revoke was a guessed
//! argument shape, and the value was gone.
//!
//! So this uses the client the module itself uses, through the library target
//! that exists for exactly this. No mint, no cleanup path to get wrong, and the
//! credential never crosses into a shell.
//!
//! Needs the daemon running, since it dials it for the credential.

use quota_core::credential_source::CredentialSource;
use quota_core::vault_handles::VaultHandleLoader;
use quota_module::ids;
use quota_module::vault_client::VaultClient;

/// Where the daemon writes its connection file.
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
        eprintln!("this dials the daemon for a vault credential: it must be running");
        std::process::exit(2);
    }

    // The same loader the module uses, so an id spelled differently here could
    // not silently probe a credential the module would never reach.
    let loader = VaultHandleLoader::from_env();
    let handles = match loader.grok_handles() {
        Ok(handles) => handles,
        Err(error) => {
            eprintln!("the handle file is unreadable ({error}): the question is unanswered.");
            std::process::exit(2);
        }
    };
    let Some(capability) = handles.iter().find_map(|handle| handle.vault_capability()) else {
        eprintln!("no vault handle mapped for grok: nothing to probe.");
        eprintln!("this is not a clean result -- the question is unanswered.");
        std::process::exit(2);
    };

    let client = VaultClient::new(path);
    let mut credential = match client.get(capability, 120_000).await {
        Ok(credential) => credential,
        Err(error) => {
            eprintln!("vault refused the credential ({error:?}): the question is unanswered.");
            eprintln!("module id dialled: {}", ids::CREDENTIALS_MODULE_ID);
            std::process::exit(2);
        }
    };
    let bearer = match quota_core::credential_source::take_utf8_payload(&mut credential.payload) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("credential payload is not usable as a bearer ({error})");
            std::process::exit(2);
        }
    };
    println!("  bearer: resolved ({} bytes, not printed)", bearer.len());

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .expect("client");

    // THE CONTROL RUNS FIRST, and it is the whole reason this probe can conclude
    // anything. An empty body from the new endpoint means "no resets" or "this
    // credential is dead" or "grpc-web says nothing here", and those are
    // indistinguishable. The endpoint we already serve answers with real data for
    // a live credential, so a zero-byte control says the CREDENTIAL is the
    // problem and nothing about the endpoint under test.
    let control = probe(
        &http,
        &bearer,
        "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig",
    )
    .await;
    println!("  control  GetGrokCreditsConfig   {}", describe(&control));

    let subject = probe(
        &http,
        &bearer,
        "https://grok.com/grok_api_v2.GrokBuildBilling/GetRemainingResets",
    )
    .await;
    println!("  subject  GetRemainingResets     {}", describe(&subject));

    println!();
    match (&control, &subject) {
        (Ok(control_body), _) if control_body.is_empty() => {
            println!("  UNANSWERED: the control returned no bytes, so this credential is not");
            println!("  serving. Nothing here is evidence about GetRemainingResets.");
            std::process::exit(2);
        }
        (Err(error), _) => {
            println!("  UNANSWERED: the control failed ({error}), so the transport or the");
            println!("  credential is the variable rather than the endpoint under test.");
            std::process::exit(2);
        }
        (Ok(_), Ok(body)) => match parse_grpc_web(body) {
            Some((0, status)) if status.starts_with("grpc-status:0") => {
                println!("  ANSWERED, AND THE ENDPOINT IS USABLE: it accepted this bearer and");
                println!("  returned success with an EMPTY message -- this account has no banked");
                println!(
                    "  resets. That is a real reading of an account with none, not a refusal."
                );
                println!();
                println!("  NOT ENOUGH TO BUILD ON. A normalizer needs a populated response to");
                println!("  know the field numbers, and an empty message shows none of them. The");
                println!("  honest record is: reachable and authorised, shape unobserved.");
                println!("  {}", hex_preview(body));
            }
            Some((0, status)) => {
                println!("  ANSWERED: empty message with {status}, so the endpoint refused in");
                println!("  grpc terms rather than http terms. The status names why.");
                println!("  {}", hex_preview(body));
            }
            Some((len, status)) => {
                println!("  ANSWERED: {len} bytes of message with {status}. This IS the evidence");
                println!("  a normalizer would be written against.");
                println!("  {}", hex_preview(body));
            }
            None => {
                println!(
                    "  UNANSWERED: {} bytes that do not parse as grpc-web framing.",
                    body.len()
                );
                println!("  {}", hex_preview(body));
                std::process::exit(2);
            }
        },
        (Ok(_), Err(error)) => {
            println!("  ANSWERED: the control carries data and the subject failed ({error}),");
            println!("  so the refusal is about this endpoint rather than the credential.");
        }
    }
}

/// One gRPC-web call, returning the raw frame bytes.
async fn probe(http: &reqwest::Client, bearer: &str, url: &str) -> Result<Vec<u8>, String> {
    // Empty protobuf body, length-prefixed per grpc-web: the frame header is five
    // bytes (one flag, four length) and both these methods take no fields.
    let body: Vec<u8> = vec![0, 0, 0, 0, 0];
    let response = http
        .post(url)
        // THE SAME HEADERS THE PROVIDER SENDS. A probe with a different request
        // shape measures a different request: grok refuses calls that do not look
        // like its own client, so a bare bearer would answer a question about my
        // headers rather than about the endpoint.
        .header("authorization", format!("Bearer {bearer}"))
        .header("origin", "https://grok.com")
        .header("referer", "https://grok.com/?_s=usage")
        .header("accept", "*/*")
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("x-user-agent", "connect-es/2.1.1")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("transport: {}", error.without_url()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("body: {}", error.without_url()))?;
    if !status.is_success() {
        return Err(format!("HTTP {status} ({} bytes)", bytes.len()));
    }
    Ok(bytes.to_vec())
}

fn describe(result: &Result<Vec<u8>, String>) -> String {
    match result {
        Ok(body) if body.is_empty() => "HTTP 200, no bytes at all".to_string(),
        Ok(body) => match parse_grpc_web(body) {
            Some((message_len, status)) => format!(
                "HTTP 200, {} bytes: message {} bytes, {}",
                body.len(),
                message_len,
                status
            ),
            None => format!("HTTP 200, {} bytes (not a grpc-web frame)", body.len()),
        },
        Err(error) => format!("FAILED: {error}"),
    }
}

/// Split a grpc-web response into its message length and its trailer status.
///
/// A BYTE COUNT IS NOT A DATA COUNT, and this endpoint is the case that proves
/// it: a successful reply carrying NO message is 25 bytes, all of it framing --
/// a zero-length data frame followed by a 15-byte trailer reading
/// `grpc-status:0`. Reporting "25 bytes" invites the reading that something was
/// returned, when the answer is an explicit, successful nothing.
///
/// Frames are a one-byte flag then a four-byte big-endian length; the trailer
/// frame sets the high bit of the flag.
fn parse_grpc_web(body: &[u8]) -> Option<(usize, String)> {
    let mut offset = 0usize;
    let mut message_len = 0usize;
    let mut status = "no trailer".to_string();
    while offset + 5 <= body.len() {
        let flag = body[offset];
        let len = u32::from_be_bytes([
            body[offset + 1],
            body[offset + 2],
            body[offset + 3],
            body[offset + 4],
        ]) as usize;
        let start = offset + 5;
        let end = start.checked_add(len)?;
        if end > body.len() {
            return None;
        }
        if flag & 0x80 == 0 {
            message_len += len;
        } else {
            status = String::from_utf8_lossy(&body[start..end])
                .trim()
                .to_string();
        }
        offset = end;
    }
    Some((message_len, status))
}

/// First bytes as hex, for reading a protobuf frame's shape without guessing at
/// a decoder. Bounded because this prints to a terminal.
fn hex_preview(body: &[u8]) -> String {
    body.iter()
        .take(64)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
