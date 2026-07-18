// SPDX-License-Identifier: GPL-3.0-or-later

//! AWS Signature Version 4 request signing (pure Rust: the workspace's
//! `sha2` + `hmac`).
//!
//! Scope: exactly what the S3 client needs — canonical request, string to
//! sign, the HMAC signing-key chain and the `Authorization` header, plus the
//! `YYYYMMDD'T'HHMMSS'Z'` timestamp (derived from `SystemTime` with a civil
//! date conversion, no date-crate dependency). The functions are generic over
//! region/service so the unit tests can pin them against the official AWS
//! SigV4 test-suite vectors (service `service`) *and* the S3 documentation
//! examples (service `s3`).
//!
//! Spec: "Signature Calculations for the Authorization Header" (AWS docs).
//! Every intermediate is exposed for tests; the one entry point callers need
//! is [`authorization_header`].

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// `sha256("")` — the payload hash of every body-less request (HEAD/GET).
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Everything that goes into signing one request. Headers must contain every
/// header to be signed (at minimum `host`, `x-amz-content-sha256`,
/// `x-amz-date`); all supplied headers are signed.
pub struct SignParams<'a> {
    /// HTTP method, uppercase (`HEAD`, `GET`, …).
    pub method: &'a str,
    /// Absolute URI path as sent on the wire, unencoded (e.g. `/bucket`).
    pub uri: &'a str,
    /// Query parameters, unencoded key/value pairs.
    pub query: &'a [(String, String)],
    /// Headers to sign (name, value); names case-insensitive.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the request payload ([`EMPTY_PAYLOAD_SHA256`] for none).
    pub payload_hash: &'a str,
    /// Request timestamp, `YYYYMMDD'T'HHMMSS'Z'` (see [`amz_datetime`]).
    pub datetime: &'a str,
    /// Signing region (e.g. `us-east-1`).
    pub region: &'a str,
    /// Signing service (`s3`; the AWS test suite uses `service`).
    pub service: &'a str,
    /// Access key id.
    pub access_key: &'a str,
    /// Secret access key.
    pub secret_key: &'a str,
}

/// AWS `UriEncode`: unreserved characters (`A–Z a–z 0–9 - . _ ~`) stay, all
/// else percent-encodes (uppercase hex, UTF-8 bytes); `/` stays only in
/// paths (`encode_slash = false`).
pub fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            b'/' if !encode_slash => out.push('/'),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// The sorted, encoded canonical query string (`k=v&k=v`; empty when there
/// are no parameters).
pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    pairs.sort();
    let parts: Vec<String> = pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.join("&")
}

/// Lowercased, sorted, trimmed canonical headers plus the `;`-joined signed
/// headers list.
fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut hs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    hs.sort();
    let canonical: String = hs.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed: Vec<&str> = hs.iter().map(|(k, _)| k.as_str()).collect();
    (canonical, signed.join(";"))
}

/// The canonical request (spec step 1) and the signed-headers list.
pub fn canonical_request(p: &SignParams<'_>) -> (String, String) {
    let (headers, signed) = canonical_headers(p.headers);
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        p.method,
        uri_encode(p.uri, false),
        canonical_query(p.query),
        headers,
        signed,
        p.payload_hash
    );
    (canonical, signed)
}

/// The credential scope: `date/region/service/aws4_request`.
pub fn scope(datetime: &str, region: &str, service: &str) -> String {
    format!("{}/{region}/{service}/aws4_request", &datetime[..8])
}

/// The string to sign (spec step 2) over a canonical request.
pub fn string_to_sign(datetime: &str, region: &str, service: &str, canonical: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{}\n{}",
        scope(datetime, region, service),
        hex::encode(Sha256::digest(canonical.as_bytes()))
    )
}

/// One HMAC-SHA256 link of the signing-key chain.
fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// The derived signing key (spec step 3):
/// `HMAC(HMAC(HMAC(HMAC("AWS4"+secret, date), region), service), "aws4_request")`.
pub fn signing_key(secret_key: &str, datetime: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{secret_key}").as_bytes(), &datetime.as_bytes()[..8]);
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// The final hex signature over the request.
pub fn signature(p: &SignParams<'_>) -> String {
    let (canonical, _) = canonical_request(p);
    let sts = string_to_sign(p.datetime, p.region, p.service, &canonical);
    let key = signing_key(p.secret_key, p.datetime, p.region, p.service);
    hex::encode(hmac(&key, sts.as_bytes()))
}

/// The complete `Authorization` header value.
pub fn authorization_header(p: &SignParams<'_>) -> String {
    let (canonical, signed) = canonical_request(p);
    let sts = string_to_sign(p.datetime, p.region, p.service, &canonical);
    let key = signing_key(p.secret_key, p.datetime, p.region, p.service);
    let sig = hex::encode(hmac(&key, sts.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={},Signature={}",
        p.access_key,
        scope(p.datetime, p.region, p.service),
        signed,
        sig
    )
}

/// Format Unix seconds as the SigV4 timestamp `YYYYMMDD'T'HHMMSS'Z'` (UTC).
/// Civil-date conversion after Howard Hinnant's `civil_from_days` — no date
/// crate needed; pinned by a unit test against known instants.
pub fn amz_datetime(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).expect("u64 seconds / 86400 always fits i64");
    let rem = unix_secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days (era-based, valid for all days >= 0)
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}{month:02}{d:02}T{h:02}{m:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Official AWS SigV4 test-suite credentials (public example values).
    const SUITE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    // S3 documentation-example credentials (public example values).
    const S3DOC_ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const S3DOC_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const S3DOC_DATE: &str = "20130524T000000Z";

    fn s3doc_params<'a>(
        method: &'a str,
        uri: &'a str,
        query: &'a [(String, String)],
        headers: &'a [(String, String)],
    ) -> SignParams<'a> {
        SignParams {
            method,
            uri,
            query,
            headers,
            payload_hash: EMPTY_PAYLOAD_SHA256,
            datetime: S3DOC_DATE,
            region: "us-east-1",
            service: "s3",
            access_key: S3DOC_ACCESS,
            secret_key: S3DOC_SECRET,
        }
    }

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// AWS SigV4 test suite, `get-vanilla`: GET / against
    /// example.amazonaws.com, service `service`.
    #[test]
    fn aws_test_suite_get_vanilla() {
        let headers = h(&[
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ]);
        let p = SignParams {
            method: "GET",
            uri: "/",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY_PAYLOAD_SHA256,
            datetime: "20150830T123600Z",
            region: "us-east-1",
            service: "service",
            access_key: "AKIDEXAMPLE",
            secret_key: SUITE_SECRET,
        };
        let (canonical, signed) = canonical_request(&p);
        assert_eq!(
            canonical,
            "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\n\
             host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(signed, "host;x-amz-date");
        assert_eq!(
            string_to_sign(p.datetime, p.region, p.service, &canonical),
            "AWS4-HMAC-SHA256\n20150830T123600Z\n\
             20150830/us-east-1/service/aws4_request\n\
             bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(
            signature(&p),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// S3 docs, "GET Object" example: GET /test.txt with a Range header.
    #[test]
    fn s3_doc_get_object() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", S3DOC_DATE),
        ]);
        let p = s3doc_params("GET", "/test.txt", &[], &headers);
        let (canonical, _) = canonical_request(&p);
        assert_eq!(
            string_to_sign(p.datetime, p.region, p.service, &canonical),
            "AWS4-HMAC-SHA256\n20130524T000000Z\n\
             20130524/us-east-1/s3/aws4_request\n\
             7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
        assert_eq!(
            signature(&p),
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        assert_eq!(
            authorization_header(&p),
            "AWS4-HMAC-SHA256 \
             Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// S3 docs, "GET Bucket Lifecycle" example: a valueless query parameter.
    #[test]
    fn s3_doc_get_bucket_lifecycle() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", S3DOC_DATE),
        ]);
        let query = h(&[("lifecycle", "")]);
        let p = s3doc_params("GET", "/", &query, &headers);
        assert_eq!(
            signature(&p),
            "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        );
    }

    /// S3 docs, "List Objects" example: multiple query parameters, sorted.
    #[test]
    fn s3_doc_list_objects() {
        let headers = h(&[
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", S3DOC_DATE),
        ]);
        // deliberately unsorted input: canonicalization must sort
        let query = h(&[("prefix", "J"), ("max-keys", "2")]);
        let p = s3doc_params("GET", "/", &query, &headers);
        assert_eq!(
            canonical_query(&query),
            "max-keys=2&prefix=J"
        );
        assert_eq!(
            signature(&p),
            "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        );
    }

    #[test]
    fn uri_encode_follows_the_aws_rules() {
        assert_eq!(uri_encode("abc-._~XYZ019", true), "abc-._~XYZ019");
        assert_eq!(uri_encode("a b+c", true), "a%20b%2Bc");
        assert_eq!(uri_encode("/path/to key", false), "/path/to%20key");
        assert_eq!(uri_encode("/slash", true), "%2Fslash");
        // UTF-8 percent-encodes per byte, uppercase hex
        assert_eq!(uri_encode("é", true), "%C3%A9");
    }

    #[test]
    fn amz_datetime_formats_known_instants() {
        // 2013-05-24T00:00:00Z
        assert_eq!(amz_datetime(1_369_353_600), "20130524T000000Z");
        // 2015-08-30T12:36:00Z (the test-suite instant)
        assert_eq!(amz_datetime(1_440_938_160), "20150830T123600Z");
        // epoch + leap-year day: 2024-02-29T23:59:59Z
        assert_eq!(amz_datetime(1_709_251_199), "20240229T235959Z");
        // 1970-01-01T00:00:00Z
        assert_eq!(amz_datetime(0), "19700101T000000Z");
    }
}
