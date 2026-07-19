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
        let path = self.bucket_path();
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
                return Err(too_many_objects());
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
///
/// Deliberately strict: a 200 whose body is not recognizably a
/// `ListBucketResult` (captive portal, error page, a 204-style empty body,
/// a namespaced/attributed dialect this minimal parser cannot read) is a
/// hard error — an *invented empty listing* would tell the user their
/// backups are gone.
pub(crate) fn parse_list_page(body: &str) -> Result<ListPage, S3Error> {
    let bad = |what: &str| S3Error::Protocol(format!("listing response: {what}"));
    // the root element may carry attributes (xmlns), so match the open
    // bracket + name only
    if !body.contains("<ListBucketResult") {
        return Err(bad("not a ListObjectsV2 result"));
    }
    let mut objects = Vec::new();
    // `outside` collects the body text NOT inside <Contents> blocks: the
    // page-level fields (IsTruncated, NextContinuationToken) are searched
    // only there, so escaped-and-decoded or hostile KEY content can never
    // impersonate them
    let mut outside = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("<Contents>") {
        outside.push_str(&rest[..start]);
        let after_open = &rest[start + "<Contents>".len()..];
        let Some(end) = after_open.find("</Contents>") else {
            // a dangling open tag means the body was cut or malformed —
            // rejecting beats silently dropping the tail
            return Err(bad("unterminated <Contents>"));
        };
        let contents = &after_open[..end];
        rest = &after_open[end + "</Contents>".len()..];
        let key = element(contents, "Key")
            .map(|(inner, _)| inner)
            .ok_or_else(|| bad("<Contents> without <Key>"))?;
        let key = unescape_xml(key)?;
        if key.is_empty() {
            // an empty key would render as a ghost row with no label
            return Err(bad("empty <Key>"));
        }
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
            return Err(too_many_objects());
        }
    }
    outside.push_str(rest);
    // any <Contents…> spelling the exact-tag loop above could not consume
    // (attributes, namespace prefix, self-closing) would otherwise vanish
    // silently — reject the whole page instead of under-reporting backups
    if outside.contains("<Contents") {
        return Err(bad("unsupported <Contents> element form"));
    }
    let truncated = element(&outside, "IsTruncated")
        .map(|(inner, _)| inner.trim() == "true")
        .unwrap_or(false);
    // the token only means something on a truncated page (a missing one
    // there would silently fake a complete listing)
    let next = if truncated {
        let (inner, _) = element(&outside, "NextContinuationToken")
            .ok_or_else(|| bad("truncated without a continuation token"))?;
        Some(unescape_xml(inner)?)
    } else {
        None
    };
    Ok(ListPage { objects, next })
}

/// The shared over-cap error (one wording for the per-page and the
/// cumulative check).
fn too_many_objects() -> S3Error {
    S3Error::Protocol(format!("listing exceeds {MAX_OBJECTS} objects"))
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

/// Whether `c` is a legal XML-1.0 character (tab/LF/CR plus the printable
/// ranges). NUL and the other C0 controls are illegal — smuggled into a UI
/// label or JSON they are at best corruption, at worst an injection vector.
fn xml_char_legal(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&c)
        || ('\u{E000}'..='\u{FFFD}').contains(&c)
        || c >= '\u{10000}'
}

/// Decode the XML escapes S3 may emit in element content: the five
/// predefined entities plus numeric character references. A malformed or
/// unknown entity is a hard [`S3Error::Protocol`] — guessing at hostile
/// input is how parsers get lied to. The decoded output is then checked as a
/// WHOLE against [`xml_char_legal`], so an illegal character reaches us the
/// same way whether it arrived as a numeric ref (`&#1;`) or as a literal
/// control byte in the source XML — the numeric-ref path is not a special
/// case with its own guard.
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
    // one guard over the whole decoded string: literal control bytes copied
    // through above are rejected exactly like an illegal numeric ref
    if out.chars().any(|c| !xml_char_legal(c)) {
        return Err(bad());
    }
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
    if d.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    // day validated against the actual month length (a "Feb 31" must be a
    // hard reject, not a silent roll-over into March)
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = match month {
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=month_days).contains(&day) {
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
        let lbr = |inner: &str| format!("<ListBucketResult>{inner}</ListBucketResult>");
        for body in [
            // Contents cut off mid-element
            "<ListBucketResult><Contents><Key>a</Key>".to_string(),
            // missing required fields
            lbr("<Contents><Key>a</Key></Contents>"),
            lbr("<Contents><Size>1</Size><LastModified>1970-01-01T00:00:00Z</LastModified></Contents>"),
            // unparseable size / date / hostile numbers
            lbr("<Contents><Key>a</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>huge</Size></Contents>"),
            lbr("<Contents><Key>a</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>-1</Size></Contents>"),
            lbr("<Contents><Key>a</Key><LastModified>not a date</LastModified><Size>1</Size></Contents>"),
            // malformed entities must not decode to something else
            lbr("<Contents><Key>a&bogus;</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>"),
            lbr("<Contents><Key>a&#xzz;</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>"),
            lbr("<Contents><Key>a&amp</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>"),
            // an empty key would become an unlabeled ghost row
            lbr("<Contents><Key></Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>"),
        ] {
            assert!(
                matches!(parse_list_page(&body), Err(S3Error::Protocol(_))),
                "hostile body must be a protocol error: {body}"
            );
        }
    }

    /// A 200 whose body is not a listing (captive portal, HTML error page,
    /// empty 204-style body) must be a hard error — parsing it as a VALID
    /// EMPTY listing would tell the user their backups are gone.
    #[test]
    fn a_non_listing_body_is_never_a_valid_empty_listing() {
        for body in [
            "",
            "<html>captive portal login</html>",
            "<?xml version=\"1.0\"?><Error><Code>SlowDown</Code></Error>",
        ] {
            assert!(
                matches!(parse_list_page(body), Err(S3Error::Protocol(_))),
                "non-listing body must be rejected: {body:?}"
            );
        }
    }

    /// Element spellings the exact-tag scan cannot consume (attributes,
    /// namespace prefixes, self-closing) must fail loudly instead of
    /// under-reporting: silently dropping <Contents> blocks would show the
    /// user an empty/partial bucket as truth.
    #[test]
    fn unsupported_contents_forms_are_rejected_not_silently_dropped() {
        for body in [
            "<ListBucketResult><Contents xmlns=\"x\"><Key>a</Key></Contents></ListBucketResult>",
            "<s3:ListBucketResult><s3:Contents><s3:Key>a</s3:Key></s3:Contents></s3:ListBucketResult>",
            "<ListBucketResult><Contents/></ListBucketResult>",
        ] {
            assert!(
                matches!(parse_list_page(body), Err(S3Error::Protocol(_))),
                "unsupported form must be rejected: {body}"
            );
        }
    }

    /// Page-level fields are read only OUTSIDE the <Contents> blocks: a key
    /// whose decoded content spells an <IsTruncated> tag must not be able
    /// to fake a complete (or truncated) listing.
    #[test]
    fn page_fields_inside_key_content_are_ignored() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                    <NextContinuationToken>t</NextContinuationToken>\
                    <Contents><Key>a&lt;IsTruncated&gt;false&lt;/IsTruncated&gt;</Key>\
                    <LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>\
                    </ListBucketResult>";
        let page = parse_list_page(body).expect("parses");
        assert_eq!(page.next.as_deref(), Some("t"), "the real trailer wins");
    }

    #[test]
    fn an_absurd_object_count_is_capped_like_the_byte_caps_in_http() {
        let one = "<Contents><Key>k</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>";
        let body = format!(
            "<ListBucketResult>{}</ListBucketResult>",
            one.repeat(MAX_OBJECTS + 1)
        );
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
            // impossible calendar dates must reject, not roll over
            "2024-02-30T00:00:00Z", "2023-02-29T00:00:00Z", "2024-04-31T00:00:00Z",
        ] {
            assert_eq!(parse_iso8601(bad), None, "must reject {bad:?}");
        }
    }

    /// A raw C0 control byte sitting literally in a <Key> (not via a numeric
    /// character reference) must be rejected exactly like an illegal numeric
    /// ref — otherwise a NUL/control smuggled straight into the XML would flow
    /// into UI labels and JSON, bypassing the numeric-ref-only guard.
    #[test]
    fn literal_control_bytes_in_a_key_are_rejected() {
        let lbr = |inner: &str| format!("<ListBucketResult>{inner}</ListBucketResult>");
        for raw in ['\u{1}', '\u{0}', '\u{7}', '\u{1F}'] {
            let body = lbr(&format!(
                "<Contents><Key>a{raw}b</Key><LastModified>1970-01-01T00:00:00Z</LastModified><Size>1</Size></Contents>"
            ));
            assert!(
                matches!(parse_list_page(&body), Err(S3Error::Protocol(_))),
                "a literal control byte {:#x} in a key must be rejected",
                raw as u32
            );
        }
        // a literal control byte in the continuation token is rejected too
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                    <NextContinuationToken>tok\u{1}en</NextContinuationToken></ListBucketResult>";
        assert!(
            matches!(parse_list_page(body), Err(S3Error::Protocol(_))),
            "a literal control byte in the continuation token must be rejected"
        );
    }

    /// The XML-1.0-legal predicate applies to literal characters as well as
    /// numeric refs — the two paths share one guard.
    #[test]
    fn a_literal_control_byte_in_unescape_is_rejected() {
        assert!(unescape_xml("ok\u{1}bad").is_err(), "literal C0 must reject");
        assert!(unescape_xml("plain text").is_ok(), "legal text passes");
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
        // XML-illegal characters (NUL, C0 controls) must not be smuggled
        // into UI labels via numeric refs
        for illegal in ["&#0;", "&#x1F;", "&#8;", "&#xFFFF;"] {
            assert!(unescape_xml(illegal).is_err(), "must reject {illegal}");
        }
    }
}
