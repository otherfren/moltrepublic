// SPDX-License-Identifier: GPL-3.0-or-later

//! A deliberately minimal HTTP/1.1 client for the S3 module: one request per
//! connection (`Connection: close`), status line + headers + body, no
//! keep-alive, no redirects-following, no HTTP/2. S3 responses are small and
//! the caller controls every header (signing needs that), so a full HTTP
//! client dependency would buy nothing and cost graph surface.
//!
//! Body handling: `Content-Length`, `Transfer-Encoding: chunked` and
//! read-to-EOF (close-delimited) — enough for HEAD probes today and the
//! ListObjects/GET/PUT operations the backup stories add later.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use super::S3Error;

/// Deadline for one whole HTTP exchange once the connection is up (the dial
/// and TLS handshakes carry their own deadlines).
const HTTP_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on response head + body — a probe/list answer is tiny; anything
/// larger is a misbehaving server, not data we want to buffer.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

/// A parsed HTTP response.
#[derive(Debug)]
pub struct HttpResponse {
    /// The status code (e.g. `200`, `403`).
    pub status: u16,
    /// Lowercased header names with their values, response order.
    pub headers: Vec<(String, String)>,
    /// The (de-chunked) body; empty for HEAD.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header with this (lowercase) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Send one request over `stream` and read the full response.
/// `path_and_query` goes on the request line verbatim; `headers` are written
/// as given (the caller supplies `Host` and every signed header). A `HEAD`
/// request reads no body.
pub async fn roundtrip<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<HttpResponse, S3Error> {
    timeout(
        HTTP_EXCHANGE_TIMEOUT,
        exchange(stream, method, path_and_query, headers, body),
    )
    .await
    .map_err(|_| S3Error::Protocol("http exchange timed out".to_string()))?
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<HttpResponse, S3Error> {
    // --- request ---
    let mut req = format!("{method} {path_and_query} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| S3Error::Protocol(format!("http write: {e}")))?;
    if !body.is_empty() {
        stream
            .write_all(body)
            .await
            .map_err(|e| S3Error::Protocol(format!("http write body: {e}")))?;
    }
    stream
        .flush()
        .await
        .map_err(|e| S3Error::Protocol(format!("http flush: {e}")))?;

    // --- response: read until provably complete or EOF, then parse ---
    let head_only = method == "HEAD";
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // servers may RST after their last byte instead of closing
                // cleanly; accept what arrived only when it is provably
                // complete — or close-delimited, where truncation is
                // undetectable by design. Anything else is a real error:
                // a partial framed body must never parse as authoritative.
                if response_complete(&buf, head_only) || close_delimited(&buf, head_only) {
                    break;
                }
                return Err(S3Error::Protocol(format!("http read: {e}")));
            }
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE {
            return Err(S3Error::Protocol("http response exceeds 4 MiB".to_string()));
        }
        // stop as soon as the response is provably complete — a keep-alive
        // server ignoring our `Connection: close` must not stall us until
        // the timeout (close-delimited bodies still need the EOF above)
        if response_complete(&buf, head_only) {
            break;
        }
    }
    parse_response(&buf, head_only)
}

/// Cap on a streamed download's response HEAD (status + headers).
const MAX_DOWNLOAD_HEAD: usize = 64 * 1024;

/// Idle deadline for a streamed download: each read must make progress
/// within this window (a whole-exchange deadline would break large
/// objects over slow circuits; a stalled peer must still not hang us).
const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Send one `GET` and **stream** a 2xx body into `out` (the object
/// download path — bodies larger than [`MAX_RESPONSE`] never sit in
/// memory). Returns `(status, bytes streamed)`; for a non-2xx status the
/// (small, buffered) error body is discarded and `(status, 0)` returned so
/// the caller maps it to its honest class. A 2xx body must be framed by
/// `Content-Length` (S3 always does; chunked/close-delimited downloads are
/// refused — truncation would be undetectable), a declared length beyond
/// `max_bytes` is refused before a byte is written, and EOF before the
/// declared length is a hard error.
pub async fn roundtrip_download<S, W>(
    stream: &mut S,
    path_and_query: &str,
    headers: &[(String, String)],
    out: &mut W,
    max_bytes: u64,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(u16, u64), S3Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    // --- request (no body) ---
    let mut req = format!("GET {path_and_query} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    let write = async {
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| S3Error::Protocol(format!("http write: {e}")))?;
        stream
            .flush()
            .await
            .map_err(|e| S3Error::Protocol(format!("http flush: {e}")))
    };
    timeout(DOWNLOAD_IDLE_TIMEOUT, write)
        .await
        .map_err(|_| S3Error::Protocol("http write timed out".to_string()))??;

    // --- head: read until \r\n\r\n ---
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let (status, resp_headers, body_start) = loop {
        if let Some(parsed) = parse_head(&buf) {
            break parsed;
        }
        if buf.len() > MAX_DOWNLOAD_HEAD {
            return Err(S3Error::Protocol("http response head exceeds 64 KiB".to_string()));
        }
        let n = timeout(DOWNLOAD_IDLE_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| S3Error::Protocol("http read timed out".to_string()))?
            .map_err(|e| S3Error::Protocol(format!("http read: {e}")))?;
        if n == 0 {
            return Err(S3Error::Protocol(
                "http response head is incomplete or malformed".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    if !(200..=299).contains(&status) {
        // error bodies are small; drain them bounded and discard
        let mut drained = buf.len();
        loop {
            let n = timeout(DOWNLOAD_IDLE_TIMEOUT, stream.read(&mut chunk))
                .await
                .map_err(|_| S3Error::Protocol("http read timed out".to_string()))?
                .unwrap_or(0);
            if n == 0 {
                return Ok((status, 0));
            }
            drained += n;
            if drained > MAX_RESPONSE {
                return Err(S3Error::Protocol("http response exceeds 4 MiB".to_string()));
            }
        }
    }

    // --- 2xx body: Content-Length-framed streaming ---
    if is_chunked(&resp_headers) {
        return Err(S3Error::Protocol(
            "chunked download body is not supported (no Content-Length)".to_string(),
        ));
    }
    let Some(total) = content_length(&resp_headers) else {
        return Err(S3Error::Protocol(
            "download without a Content-Length — truncation would be undetectable".to_string(),
        ));
    };
    let total = u64::try_from(total).unwrap_or(u64::MAX);
    if total > max_bytes {
        return Err(S3Error::Protocol(format!(
            "object is {total} bytes — beyond the {max_bytes}-byte cap"
        )));
    }
    let mut written: u64 = 0;
    // whatever body bytes arrived with the head go out first
    let leftover = &buf[body_start..];
    let take = usize::try_from(total.min(u64::try_from(leftover.len()).unwrap_or(u64::MAX)))
        .unwrap_or(leftover.len());
    if take > 0 {
        out.write_all(&leftover[..take])
            .await
            .map_err(|e| S3Error::Protocol(format!("writing the download: {e}")))?;
        written += u64::try_from(take).unwrap_or(0);
        progress(written, Some(total));
    }
    while written < total {
        let n = timeout(DOWNLOAD_IDLE_TIMEOUT, stream.read(&mut chunk))
            .await
            .map_err(|_| S3Error::Protocol("http read timed out".to_string()))?
            .map_err(|e| S3Error::Protocol(format!("http read: {e}")))?;
        if n == 0 {
            return Err(S3Error::Protocol(format!(
                "http body was truncated ({written} of {total} bytes)"
            )));
        }
        let remaining = usize::try_from(total - written).unwrap_or(usize::MAX);
        let use_n = n.min(remaining);
        out.write_all(&chunk[..use_n])
            .await
            .map_err(|e| S3Error::Protocol(format!("writing the download: {e}")))?;
        written += u64::try_from(use_n).unwrap_or(0);
        progress(written, Some(total));
    }
    out.flush()
        .await
        .map_err(|e| S3Error::Protocol(format!("writing the download: {e}")))?;
    Ok((status, written))
}

/// The index just past the head's `\r\n\r\n`, once fully received.
fn head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// A parsed response head: status, lowercased headers, body offset.
type ParsedHead = (u16, Vec<(String, String)>, usize);

/// Parse the (complete) head.
fn parse_head(raw: &[u8]) -> Option<ParsedHead> {
    let end = head_end(raw)?;
    let head = std::str::from_utf8(&raw[..end - 4]).ok()?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Some((status, headers, end))
}

fn is_chunked(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"))
}

fn content_length(headers: &[(String, String)]) -> Option<usize> {
    headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
}

/// Whether `raw` already holds a provably complete response: a full head,
/// plus (for non-HEAD) a fully-arrived framed body. A close-delimited body
/// (neither `Content-Length` nor chunked) is never provably complete — only
/// the peer's EOF ends it.
fn response_complete(raw: &[u8], head_only: bool) -> bool {
    let Some((_, headers, body_start)) = parse_head(raw) else {
        return false;
    };
    if head_only {
        return true;
    }
    let rest = &raw[body_start..];
    if is_chunked(&headers) {
        dechunk(rest).is_ok()
    } else if let Some(len) = content_length(&headers) {
        rest.len() >= len
    } else {
        false
    }
}

/// Whether `raw` holds a complete head announcing a close-delimited body.
fn close_delimited(raw: &[u8], head_only: bool) -> bool {
    !head_only
        && parse_head(raw)
            .is_some_and(|(_, headers, _)| !is_chunked(&headers) && content_length(&headers).is_none())
}

/// Parse a full HTTP/1.1 response held in `raw`.
fn parse_response(raw: &[u8], head_only: bool) -> Result<HttpResponse, S3Error> {
    let (status, headers, body_start) = parse_head(raw).ok_or_else(|| {
        S3Error::Protocol("http response head is incomplete or malformed".to_string())
    })?;
    let rest = &raw[body_start..];
    let body = if head_only {
        Vec::new()
    } else if is_chunked(&headers) {
        dechunk(rest)?
    } else if let Some(len) = content_length(&headers) {
        if rest.len() < len {
            return Err(S3Error::Protocol("http body was truncated".to_string()));
        }
        rest[..len].to_vec()
    } else {
        rest.to_vec() // close-delimited
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// Decode a `Transfer-Encoding: chunked` body.
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, S3Error> {
    let mut body = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| S3Error::Protocol("chunked body was truncated".to_string()))?;
        let size_str = std::str::from_utf8(&rest[..line_end])
            .map_err(|_| S3Error::Protocol("bad chunk size".to_string()))?;
        // chunk extensions (";…") are allowed, ignore them
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| S3Error::Protocol(format!("bad chunk size: {size_str}")))?;
        // reject absurd sizes BEFORE any arithmetic — a hostile
        // `ffffffffffffffff` must not overflow `size + 2` or slice-panic
        if size > MAX_RESPONSE {
            return Err(S3Error::Protocol("http response exceeds 4 MiB".to_string()));
        }
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body); // trailers, if any, are ignored
        }
        if rest.len() < size + 2 {
            return Err(S3Error::Protocol("chunked body was truncated".to_string()));
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..]; // skip the chunk's trailing CRLF
        if body.len() > MAX_RESPONSE {
            return Err(S3Error::Protocol("http response exceeds 4 MiB".to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Amz-Request-Id: abc\r\n\r\nhello";
        let r = parse_response(raw, false).expect("parses");
        assert_eq!(r.status, 200);
        assert_eq!(r.header("x-amz-request-id"), Some("abc"));
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let r = parse_response(raw, false).expect("parses");
        assert_eq!(r.body, b"Wikipedia");
    }

    #[test]
    fn head_response_has_no_body_even_with_content_length() {
        // HEAD advertises the body it would have sent — but sends none
        let raw = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 243\r\n\r\n";
        let r = parse_response(raw, true).expect("parses");
        assert_eq!(r.status, 403);
        assert!(r.body.is_empty());
    }

    #[test]
    fn truncated_head_is_a_protocol_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Le";
        assert!(matches!(
            parse_response(raw, false),
            Err(S3Error::Protocol(_))
        ));
    }

    #[test]
    fn truncated_body_is_a_protocol_error() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        assert!(matches!(
            parse_response(raw, false),
            Err(S3Error::Protocol(_))
        ));
    }

    #[test]
    fn hostile_chunk_size_is_an_error_not_a_panic() {
        // a chunk size that would overflow `size + 2` / slice out of range
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    ffffffffffffffff\r\nx\r\n0\r\n\r\n";
        assert!(matches!(
            parse_response(raw, false),
            Err(S3Error::Protocol(_))
        ));
    }

    #[test]
    fn completeness_is_provable_only_for_framed_bodies() {
        // content-length: complete exactly when the body arrived in full
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert!(response_complete(full, false));
        assert!(!response_complete(&full[..full.len() - 1], false));
        // chunked: complete only with the terminating 0-chunk
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                        4\r\nWiki\r\n0\r\n\r\n";
        assert!(response_complete(chunked, false));
        assert!(!response_complete(&chunked[..chunked.len() - 7], false));
        // close-delimited: never provably complete, but recognized as such
        let close = b"HTTP/1.1 200 OK\r\n\r\npartial";
        assert!(!response_complete(close, false));
        assert!(close_delimited(close, false));
        assert!(!close_delimited(full, false));
        // HEAD: complete at the end of the head, whatever it announces
        let head = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 243\r\n\r\n";
        assert!(response_complete(head, true));
        assert!(!response_complete(&head[..head.len() - 2], true));
    }
}
