// SPDX-License-Identifier: GPL-3.0-or-later

//! ListObjectsV2 for the S3 module (mock_todo story 8): one thin operation
//! over the operation-agnostic [`S3Client::request`] core, plus a minimal
//! hand-rolled parser for the XML response — consistent with the module's
//! posture (`http.rs`): S3 answers are small and shape-known, so a full XML
//! dependency would buy nothing and cost graph surface.
//!
//! The parser is written for hostile input: element scans never panic,
//! entity decoding rejects malformed escapes, sizes/dates must parse, a
//! truncated page without a continuation token is an error (silent
//! truncation would fake a complete listing), and the object count is
//! capped like `http.rs` caps the byte size — a misbehaving server yields
//! an honest [`S3Error::Protocol`], never invented data.

use super::{S3Client, S3Error};

/// One listed object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Object {
    /// The object key (decoded).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// `LastModified`, unix seconds (clamped to 0 for pre-1970 nonsense).
    pub modified: u64,
}

/// Hard cap on listed objects, in one page and across pages — with
/// retention pruning a real backup bucket holds a handful of objects per
/// workspace; anything beyond this is a misbehaving server or the wrong
/// bucket, not data we want to buffer.
const MAX_OBJECTS: usize = 10_000;

/// Hard cap on pagination round-trips (a server replaying the same
/// continuation token must not loop us forever).
const MAX_PAGES: usize = 32;

/// One parsed response page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ListPage {
    pub(crate) objects: Vec<S3Object>,
    /// `NextContinuationToken` when the page was truncated.
    pub(crate) next: Option<String>,
}

impl S3Client {
    /// List the bucket's objects under `prefix` (ListObjectsV2, path-style),
    /// following continuation tokens until complete. Every failure carries
    /// its honest class — connect/TLS via [`S3Client::request`], HTTP status
    /// like the probe (403 credentials, 404 missing bucket), and
    /// [`S3Error::Protocol`] for anything that does not parse as a listing.
    pub async fn list_objects(&self, prefix: &str) -> Result<Vec<S3Object>, S3Error> {
        let path = format!("{}/{}", self.config.endpoint.base_path, self.config.bucket);
        let mut out: Vec<S3Object> = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("prefix".to_string(), prefix.to_string()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".to_string(), t.clone()));
            }
            let resp = self.request("GET", &path, &query, &[]).await?;
            if !(200..=299).contains(&resp.status) {
                return Err(self.status_error(resp.status));
            }
            let body = std::str::from_utf8(&resp.body)
                .map_err(|_| S3Error::Protocol("listing response is not UTF-8".to_string()))?;
            let page = parse_list_page(body)?;
            if out.len() + page.objects.len() > MAX_OBJECTS {
                return Err(S3Error::Protocol(format!(
                    "listing exceeds {MAX_OBJECTS} objects"
                )));
            }
            out.extend(page.objects);
            match page.next {
                Some(t) => token = Some(t),
                None => return Ok(out),
            }
        }
        Err(S3Error::Protocol(format!(
            "listing did not complete within {MAX_PAGES} pages"
        )))
    }
}

/// Parse one ListObjectsV2 response body.
pub(crate) fn parse_list_page(body: &str) -> Result<ListPage, S3Error> {
    let bad = |what: &str| S3Error::Protocol(format!("listing response: {what}"));
    let mut objects = Vec::new();
    let mut rest = body;
    while let Some((contents, after)) = element(rest, "Contents") {
        rest = after;
        let key = element(contents, "Key")
            .map(|(inner, _)| inner)
            .ok_or_else(|| bad("<Contents> without <Key>"))?;
        let key = unescape_xml(key)?;
        let size: u64 = element(contents, "Size")
            .map(|(inner, _)| inner.trim())
            .ok_or_else(|| bad("<Contents> without <Size>"))?
            .parse()
            .map_err(|_| bad("unparseable <Size>"))?;
        let modified = element(contents, "LastModified")
            .map(|(inner, _)| inner.trim())
            .ok_or_else(|| bad("<Contents> without <LastModified>"))
            .and_then(|s| parse_iso8601(s).ok_or_else(|| bad("unparseable <LastModified>")))?;
        objects.push(S3Object {
            key,
            size,
            modified,
        });
        if objects.len() > MAX_OBJECTS {
            return Err(S3Error::Protocol(format!(
                "listing exceeds {MAX_OBJECTS} objects"
            )));
        }
    }
    // the loop above stops when no complete <Contents>…</Contents> is left;
    // a dangling open tag means the body was cut or malformed — rejecting
    // beats silently dropping the tail
    if rest.contains("<Contents>") {
        return Err(bad("unterminated <Contents>"));
    }
    let truncated = element(body, "IsTruncated")
        .map(|(inner, _)| inner.trim() == "true")
        .unwrap_or(false);
    let next = match element(body, "NextContinuationToken") {
        Some((inner, _)) => Some(unescape_xml(inner)?),
        None => None,
    };
    if truncated && next.is_none() {
        // accepting the partial page would fake a complete listing
        return Err(bad("truncated without a continuation token"));
    }
    Ok(ListPage {
        objects,
        next: if truncated { next } else { None },
    })
}

/// Find the first `<name>…</name>` element; returns `(inner, rest after the
/// closing tag)`. Exact-tag match (`<Key>` never matches `<KeyCount>`);
/// attributes never occur on the elements we read (S3 puts its `xmlns` on
/// the root only). `None` when the element is absent or unterminated (the
/// listing parser turns a dangling `<Contents>` into a hard error).
fn element<'a>(xml: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some((&xml[start..end], &xml[end + close.len()..]))
}

/// Decode the XML escapes S3 may emit in element content: the five
/// predefined entities plus numeric character references. A malformed or
/// unknown entity is a hard [`S3Error::Protocol`] — guessing at hostile
/// input is how parsers get lied to.
fn unescape_xml(s: &str) -> Result<String, S3Error> {
    let bad = || S3Error::Protocol("listing response: malformed XML entity".to_string());
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let entity_rest = &rest[i + 1..];
        let end = entity_rest.find(';').ok_or_else(bad)?;
        let entity = &entity_rest[..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let digits = entity.strip_prefix('#').ok_or_else(bad)?;
                let code = match digits.strip_prefix('x').or_else(|| digits.strip_prefix('X')) {
                    Some(hex) => u32::from_str_radix(hex, 16).map_err(|_| bad())?,
                    None => digits.parse::<u32>().map_err(|_| bad())?,
                };
                out.push(char::from_u32(code).ok_or_else(bad)?);
            }
        }
        rest = &entity_rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Parse S3's `LastModified` timestamp (`YYYY-MM-DDTHH:MM:SS[.fff]Z`, UTC)
/// into unix seconds — the inverse of [`super::sigv4::amz_datetime`]'s
/// civil-date conversion (Howard Hinnant's `days_from_civil`), no date
/// crate. `None` for anything malformed; pre-1970 clamps to 0.
fn parse_iso8601(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.strip_suffix('Z')?;
    // fractional seconds are ignored (whole-second resolution is plenty
    // for "minutes since the last backup")
    let time = time.split_once('.').map_or(time, |(t, frac)| {
        if frac.bytes().all(|b| b.is_ascii_digit()) {
            t
        } else {
            "" // malformed fraction → fails the HH:MM:SS parse below
        }
    });
    let mut t = time.split(':');
    let (h, m, sec): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if t.next().is_some() || h > 23 || m > 59 || sec > 60 {
        return None;
    }
    // days_from_civil
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era.checked_mul(146_097)?.checked_add(doe)?.checked_sub(719_468)?;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::try_from(h * 3600 + m * 60 + sec).ok()?)?;
    Some(u64::try_from(secs).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_listing() {
        // shaped after the AWS ListObjectsV2 documentation example, plus an
        // escaped key and an Owner block (which must not confuse the scan)
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>molt-bucket</Name><Prefix>molt/</Prefix><KeyCount>2</KeyCount>
  <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Contents>
    <Key>molt/aabb/001752800000.molt.enc</Key>
    <LastModified>2013-05-24T00:00:00.000Z</LastModified>
    <ETag>&quot;fba9dede5f27731c9771645a39863328&quot;</ETag>
    <Size>4096</Size>
    <Owner><ID>abc</ID><DisplayName>x</DisplayName></Owner>
    <StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>molt/we &amp; they &#x2764;.bin</Key>
    <LastModified>2015-08-30T12:36:00Z</LastModified>
    <Size>7</Size>
  </Contents>
</ListBucketResult>"#;
        let page = parse_list_page(body).expect("parses");
        assert_eq!(page.next, None);
        assert_eq!(
            page.objects,
            vec![
                S3Object {
                    key: "molt/aabb/001752800000.molt.enc".to_string(),
                    size: 4096,
                    modified: 1_369_353_600, // pinned by the sigv4 vector too
                },
                S3Object {
                    key: "molt/we & they \u{2764}.bin".to_string(),
                    size: 7,
                    modified: 1_440_938_160,
                },
            ]
        );
    }

    #[test]
    fn an_empty_listing_yields_no_objects() {
        let body = r#"<?xml version="1.0"?><ListBucketResult>
  <Name>b</Name><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated>
</ListBucketResult>"#;
        let page = parse_list_page(body).expect("parses");
        assert!(page.objects.is_empty());
        assert_eq!(page.next, None);
    }

    #[test]
    fn a_truncated_page_carries_its_token() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                    <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>\
                    <Contents><Key>a</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>\
                    </ListBucketResult>";
        let page = parse_list_page(body).expect("parses");
        assert_eq!(page.objects.len(), 1);
        assert_eq!(
            page.next.as_deref(),
            Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=")
        );
    }

    #[test]
    fn truncation_without_a_token_is_rejected_not_silently_partial() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>";
        assert!(matches!(
            parse_list_page(body),
            Err(S3Error::Protocol(_))
        ));
    }

    #[test]
    fn hostile_or_truncated_xml_is_an_error_never_a_panic_or_guess() {
        for body in [
            // Contents cut off mid-element
            "<ListBucketResult><Contents><Key>a</Key>",
            // missing required fields
            "<x><Contents><Key>a</Key></Contents></x>",
            "<x><Contents><Size>1</Size><LastModified>1970-01-01T00:00:00Z</LastModified></Contents></x>",
            // unparseable size / date / hostile numbers
            "<x><Contents><Key>a</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>huge</Size></Contents></x>",
            "<x><Contents><Key>a</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>-1</Size></Contents></x>",
            "<x><Contents><Key>a</Key><LastModified>not a date</LastModified><Size>1</Size></Contents></x>",
            // malformed entities must not decode to something else
            "<x><Contents><Key>a&bogus;</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents></x>",
            "<x><Contents><Key>a&#xzz;</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents></x>",
            "<x><Contents><Key>a&amp</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents></x>",
        ] {
            assert!(
                matches!(parse_list_page(body), Err(S3Error::Protocol(_))),
                "hostile body must be a protocol error: {body}"
            );
        }
    }

    #[test]
    fn an_absurd_object_count_is_capped_like_the_byte_caps_in_http() {
        let one = "<Contents><Key>k</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>";
        let body = format!("<x>{}</x>", one.repeat(MAX_OBJECTS + 1));
        assert!(matches!(
            parse_list_page(&body),
            Err(S3Error::Protocol(_))
        ));
    }

    #[test]
    fn iso8601_parses_the_s3_shapes_and_rejects_garbage() {
        // the same instants amz_datetime pins, round-tripped back
        assert_eq!(parse_iso8601("2013-05-24T00:00:00.000Z"), Some(1_369_353_600));
        assert_eq!(parse_iso8601("2015-08-30T12:36:00Z"), Some(1_440_938_160));
        assert_eq!(parse_iso8601("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        // pre-1970 clamps instead of wrapping
        assert_eq!(parse_iso8601("1969-12-31T23:00:00Z"), Some(0));
        for bad in [
            "", "2024-01-01", "2024-01-01T00:00:00", "2024-13-01T00:00:00Z",
            "2024-00-10T00:00:00Z", "2024-01-32T00:00:00Z", "2024-01-01T24:00:00Z",
            "2024-01-01T00:61:00Z", "9999999999999-01-01T00:00:00Z",
            "2024-01-01T00:00:00.abcZ", "2024-01-01T00:00:00+02:00",
        ] {
            assert_eq!(parse_iso8601(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn entity_unescape_covers_predefined_and_numeric_refs() {
        assert_eq!(unescape_xml("plain").expect("ok"), "plain");
        assert_eq!(
            unescape_xml("&lt;a&gt; &amp; &quot;b&quot; &apos;c&apos;").expect("ok"),
            "<a> & \"b\" 'c'"
        );
        assert_eq!(unescape_xml("&#65;&#x42;").expect("ok"), "AB");
        assert!(unescape_xml("&#x110000;").is_err(), "beyond char range");
        assert!(unescape_xml("dangling &").is_err());
    }
}
