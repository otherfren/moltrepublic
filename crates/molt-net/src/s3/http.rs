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

use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use super::S3Error;

/// Deadline for one whole HTTP exchange once the connection is up (the dial
/// and TLS handshakes carry their own deadlines). Covers head + a *small*
/// body: the streaming upload/download paths do NOT ride this — a large blob
/// would deterministically time out (see [`roundtrip_upload`]).
const HTTP_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on response head + body — a probe/list answer is tiny; anything
/// larger is a misbehaving server, not data we want to buffer.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

/// Idle deadline for one body-write slice on the streaming upload path: each
/// bounded `write_all` must make progress within this window. A large backup
/// blob then rides the SUM of per-slice windows, never a single
/// whole-exchange cap it would blow through over a slow (Tor) circuit.
pub const UPLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Body-write slice size for the streaming upload: small enough that each
/// slice drains well within [`UPLOAD_IDLE_TIMEOUT`] on any real circuit.
const UPLOAD_CHUNK: usize = 64 * 1024;

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
    read_full_response(stream, head_only).await
}

/// Read one whole HTTP response (head + framed/close-delimited body) into
/// memory and parse it. Shared by the buffered [`roundtrip`] and the response
/// tail of [`roundtrip_upload`]; the caller bounds it in time.
async fn read_full_response<S: AsyncRead + Unpin>(
    stream: &mut S,
    head_only: bool,
) -> Result<HttpResponse, S3Error> {
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

/// `write_all` a slice under an idle deadline: it must make progress within
/// `idle` or the write is treated as stalled. Bounded slices give the upload
/// path a per-write floor instead of one cap over the whole (size-dependent)
/// exchange.
async fn write_all_idle<S: AsyncWrite + Unpin>(
    stream: &mut S,
    data: &[u8],
    idle: Duration,
    what: &str,
) -> Result<(), S3Error> {
    timeout(idle, stream.write_all(data))
        .await
        .map_err(|_| S3Error::Protocol(format!("{what} timed out")))?
        .map_err(|e| S3Error::Protocol(format!("{what}: {e}")))
}

/// Send one request with a (possibly large) `body` and read the response —
/// the object *upload* path (`PUT`). Unlike [`roundtrip`], the body is
/// written in bounded slices, each under its own `write_idle` deadline, so a
/// realistically-sized backup blob rides the sum of per-slice windows rather
/// than one whole-exchange cap it would blow through over a slow circuit. The
/// small head + response tail keep a single whole-exchange cap. A stalled
/// peer (no progress within `write_idle`) still fails, promptly.
pub async fn roundtrip_upload<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
    write_idle: Duration,
) -> Result<HttpResponse, S3Error> {
    // --- request head ---
    let mut req = format!("{method} {path_and_query} HTTP/1.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    write_all_idle(stream, req.as_bytes(), write_idle, "http write").await?;

    // --- body in bounded, individually idle-bounded slices ---
    for slice in body.chunks(UPLOAD_CHUNK) {
        write_all_idle(stream, slice, write_idle, "http write body").await?;
    }
    timeout(write_idle, stream.flush())
        .await
        .map_err(|_| S3Error::Protocol("http flush timed out".to_string()))?
        .map_err(|e| S3Error::Protocol(format!("http flush: {e}")))?;

    // --- response: small head + body, one whole-exchange cap ---
    timeout(HTTP_EXCHANGE_TIMEOUT, read_full_response(stream, false))
        .await
        .map_err(|_| S3Error::Protocol("http exchange timed out".to_string()))?
}

/// Cap on a streamed download's response HEAD (status + headers).
const MAX_DOWNLOAD_HEAD: usize = 64 * 1024;

/// Time bounds for a streamed download. An idle timeout alone lets a server
/// dribble one byte per (idle − ε) forever; the overall floor derived from
/// [`DownloadBounds::overall`] caps the total so a byte-at-a-time peer cannot
/// hold the restore task effectively unbounded.
#[derive(Debug, Clone, Copy)]
pub struct DownloadBounds {
    /// Each read/write must make progress within this window.
    pub idle: Duration,
    /// Slack before the minimum-throughput floor starts to apply.
    pub grace: Duration,
    /// Minimum sustained bytes/second after the grace period (a value of 0
    /// disables the floor).
    pub min_throughput_bps: u64,
}

impl DownloadBounds {
    /// Production bounds: a 30 s idle window (large objects over a slow Tor
    /// circuit are fine), a 30 s grace, and a 1 KiB/s throughput floor.
    pub const PRODUCTION: DownloadBounds = DownloadBounds {
        idle: Duration::from_secs(30),
        grace: Duration::from_secs(30),
        min_throughput_bps: 1024,
    };

    /// The overall deadline for moving up to `max_bytes`: `grace` plus the
    /// time the throughput floor would allow for `max_bytes`. `min_throughput`
    /// of 0 means "no floor" (an effectively unbounded deadline).
    fn overall(&self, max_bytes: u64) -> Duration {
        if self.min_throughput_bps == 0 {
            return Duration::from_secs(u64::MAX);
        }
        self.grace + Duration::from_millis(max_bytes.saturating_mul(1000) / self.min_throughput_bps)
    }
}

/// Send one `GET` and **stream** a 2xx body into `out` (the object
/// download path — bodies larger than [`MAX_RESPONSE`] never sit in
/// memory). Returns `(status, bytes streamed)`; for a non-2xx status the
/// (small, buffered) error body is drained and discarded and `(status, 0)`
/// returned — never letting the drain mask the honest status with a timeout —
/// so the caller maps it to its honest class. A 2xx body must be framed by
/// `Content-Length` (S3 always does; chunked/close-delimited downloads are
/// refused — truncation would be undetectable), a declared length beyond
/// `max_bytes` is refused before a byte is written, and EOF before the
/// declared length is a hard error. `bounds` gives both an idle window (each
/// read makes progress) and an overall minimum-throughput floor (a dribbling
/// server cannot hold us unbounded).
pub async fn roundtrip_download<S, W>(
    stream: &mut S,
    path_and_query: &str,
    headers: &[(String, String)],
    out: &mut W,
    max_bytes: u64,
    bounds: DownloadBounds,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(u16, u64), S3Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    let idle = bounds.idle;
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
    timeout(idle, write)
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
        let n = timeout(idle, stream.read(&mut chunk))
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

    // the body/drain phase is bounded both by the per-read idle window and by
    // an overall minimum-throughput floor from here on
    let started = Instant::now();
    let overall = bounds.overall(max_bytes);

    if !(200..=299).contains(&status) {
        return drain_error_body(stream, status, &buf, body_start, &resp_headers, idle, started, overall)
            .await;
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
        if started.elapsed() > overall {
            return Err(S3Error::Protocol(format!(
                "download too slow — below the {} B/s floor ({written} of {total} bytes)",
                bounds.min_throughput_bps
            )));
        }
        let n = timeout(idle, stream.read(&mut chunk))
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

/// Drain (and discard) a non-2xx error body, then return `(status, 0)`. The
/// honest HTTP status is the answer here — draining is only politeness before
/// the connection closes — so nothing masks it: a keep-alive server that
/// ignores `Connection: close` is stopped as soon as `Content-Length` / the
/// chunked terminator says the body is complete, and an idle timeout, read
/// error, or the overall floor all resolve to `(status, 0)` rather than a
/// [`S3Error::Protocol`] that would hide the real status.
#[allow(clippy::too_many_arguments)]
async fn drain_error_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    status: u16,
    buf: &[u8],
    body_start: usize,
    resp_headers: &[(String, String)],
    idle: Duration,
    started: Instant,
    overall: Duration,
) -> Result<(u16, u64), S3Error> {
    let mut chunk = [0u8; 8192];
    let mut have = buf.len() - body_start; // body bytes already read with the head
    // Content-Length-framed: read exactly the declared body, no more — a
    // keep-alive server must not stall us past the last body byte
    if let Some(len) = content_length(resp_headers) {
        while have < len && have <= MAX_RESPONSE && started.elapsed() <= overall {
            match timeout(idle, stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break, // EOF/error/idle: status wins
                Ok(Ok(n)) => have += n,
            }
        }
        return Ok((status, 0));
    }
    // chunked: read until the terminator parses (dechunk succeeds)
    if is_chunked(resp_headers) {
        let mut acc = buf[body_start..].to_vec();
        while dechunk(&acc).is_err() && acc.len() <= MAX_RESPONSE && started.elapsed() <= overall {
            match timeout(idle, stream.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => acc.extend_from_slice(&chunk[..n]),
            }
        }
        return Ok((status, 0));
    }
    // close-delimited: read to EOF, bounded by MAX_RESPONSE and the floor
    let mut drained = buf.len();
    while drained <= MAX_RESPONSE && started.elapsed() <= overall {
        match timeout(idle, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => drained += n,
        }
    }
    Ok((status, 0))
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
