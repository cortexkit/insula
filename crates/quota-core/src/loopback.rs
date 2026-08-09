//! A loopback HTTP server for provider tests, and the one correct way to read
//! a request off it.
//!
//! Four provider modules each grew their own copy of this, and three of the four
//! read the request with a single `stream.read()`. That returns whatever one TCP
//! segment happened to carry, not the whole request -- the kernel is free to
//! split a request across segments, and reqwest may write headers and body
//! separately, so a single read is a PREFIX of unpredictable length.
//!
//! The severity depends on what the test then asserts, and the two directions
//! are not symmetrical:
//!
//! - `assert!(request.contains(..))` on a truncated read FAILS, so it surfaces
//!   as an intermittent red run. Bad, but it announces itself.
//! - `assert!(!request.contains(..))` on a truncated read PASSES, and passes
//!   for a reason unrelated to the claim. A test asserting that a served token
//!   did not leak into the request would be satisfied by reading none of the
//!   request at all.
//!
//! Those negative assertions are exactly the ones worth having -- they are how
//! a credential-isolation claim is proven -- so the read has to be complete
//! before they mean anything.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Read one complete HTTP request: headers, then `content-length` bytes.
///
/// Loops until the body is fully received rather than trusting one read to
/// deliver it. A request with no `content-length` is complete at the end of its
/// headers.
pub async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).to_string()
}

/// Serve one request with a fixed status and body, and hand back what was sent.
///
/// Returns the base URL and a task resolving to the complete request text. The
/// caller appends its own path, because the path is provider-specific and
/// asserting on it is often the point.
pub async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let reason = if status == 200 { "OK" } else { "Unauthorized" };
        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        request
    });
    (format!("http://{address}"), task)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request split across TCP segments is still read whole.
    ///
    /// This is the case the per-module copies got wrong, and it cannot be
    /// reproduced reliably by sending a normal request: whether the kernel
    /// splits one depends on timing and buffer state, so the bug appears as an
    /// occasional flake rather than a failure. Here the split is forced, by
    /// writing the headers, pausing, and then writing the body.
    ///
    /// Asserted on the body specifically. The headers arrive in the first
    /// segment either way, so a reader that stops after one read still sees
    /// them -- only the body distinguishes a complete read from a prefix.
    #[tokio::test]
    async fn a_request_split_across_segments_is_read_whole() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await
        });

        let body = r#"{"secret":"must-not-be-missed"}"#;
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                format!(
                    "POST /quota HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
        // Long enough that a single-read server has certainly returned with
        // only the headers by the time the body arrives.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        client.write_all(body.as_bytes()).await.unwrap();

        let request = server.await.unwrap();
        assert!(
            request.contains(body),
            "the body arrived in a later segment and was lost: {request:?}"
        );
        // Not vacuous in the other direction: a reader that somehow returned
        // everything ever sent would also pass the line above.
        assert!(request.starts_with("POST /quota "));
    }

    /// A request with no body is complete at the end of its headers.
    ///
    /// Without this the loop would wait for bytes that never come, and a GET
    /// test would hang rather than fail -- an outcome that reads as an
    /// infrastructure problem rather than a broken server.
    #[tokio::test]
    async fn a_request_without_a_body_does_not_wait_for_one() {
        let (url, request) = serve_once(200, b"{}".to_vec()).await;
        let response = reqwest::Client::new()
            .get(format!("{url}/usage"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(request.await.unwrap().starts_with("GET /usage "));
    }
}
