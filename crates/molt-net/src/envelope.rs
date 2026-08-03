// SPDX-License-Identifier: GPL-3.0-or-later

//! N3 (`docs/transport/nostr_n3_plan.md` §2): the OUTER layer of a kind-445
//! group event — the wrapper that hides the MLS frames from relays.
//!
//! Shape, decided in concept §10.11 (the CURRENT Marmot form, not the older
//! derived-keypair NIP-44 one):
//!
//! ```text
//! content = base64( nonce ‖ ChaCha20Poly1305(exporter_secret, plaintext, aad = "") )
//! ```
//!
//! One sealing, keyed by the epoch's exporter secret itself. The secret
//! authenticates nothing and grants no MLS read capability — it hides frames
//! from OUTSIDERS, never from a group member (concept §7). Opening walks the
//! current secret first and then the bounded exporter ring
//! ([`crate::mls::EXPORTER_RING_K`]); past the ring an event is
//! **epoch-opaque** and must be reported loudly (G4), never silently
//! skipped.

use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// ChaCha20-Poly1305's nonce width — the per-event random prefix.
const NONCE_LEN: usize = 12;

/// The AEAD tag width: a frame shorter than nonce+tag cannot be a sealing.
const TAG_LEN: usize = 16;

/// What went wrong opening an outer envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// The content is not `base64(nonce ‖ ciphertext‖tag)` at all — refused
    /// on shape, before any key is tried.
    #[error("malformed 445 content: {0}")]
    Shape(String),
    /// No available secret opens it: either it belongs to an epoch older
    /// than the ring reaches, or it is not ours (relay spam, a tampered
    /// frame). Both are the same observation from here — and both must be
    /// reported, never silently dropped.
    #[error("epoch-opaque: no current or ringed exporter secret opens this event")]
    EpochOpaque,
    /// Sealing failed (the RNG, or an over-long payload).
    #[error("sealing failed: {0}")]
    Seal(String),
}

/// Seal `plaintext` under `secret` into the base64 content of a kind-445
/// event. A FRESH random nonce per call: two sealings of the same bytes
/// never share ciphertext, so a relay cannot recognize repeated content.
pub fn seal_outer(secret: &[u8; 32], plaintext: &[u8]) -> Result<String, EnvelopeError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| EnvelopeError::Seal(format!("os rng unavailable: {e}")))?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload { msg: plaintext, aad: b"" },
        )
        .map_err(|_| EnvelopeError::Seal("aead rejected the payload".into()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&sealed);
    Ok(encode_base64(&out))
}

/// Open a kind-445 content with the secrets available to this node, in
/// order: the CURRENT epoch's exporter secret first (the common case, one
/// AEAD attempt), then the ring, newest first.
///
/// Shape is validated before any key is tried, so a malformed frame costs
/// nothing and is distinguishable from an unopenable one.
pub fn open_outer(secrets: &[[u8; 32]], content: &str) -> Result<Vec<u8>, EnvelopeError> {
    let raw = decode_base64(content)?;
    if raw.len() < NONCE_LEN + TAG_LEN {
        return Err(EnvelopeError::Shape(format!(
            "{} bytes is shorter than a nonce plus tag",
            raw.len()
        )));
    }
    let (nonce_bytes, sealed) = raw.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    for secret in secrets {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
        if let Ok(plaintext) = cipher.decrypt(nonce, Payload { msg: sealed, aad: b"" }) {
            return Ok(plaintext);
        }
    }
    Err(EnvelopeError::EpochOpaque)
}

/// What the MLS layer adds around a plaintext before it is sealed: the
/// `PrivateMessage` framing (group id, epoch, content type, encrypted sender
/// data), the content's signature and confirmation tag, and the AEAD tag.
/// Measured, then rounded up to a round number —
/// `group_frame_budget.rs::the_cost_model_never_under_estimates_the_real_frame`
/// is what keeps it honest.
const MLS_FRAME_OVERHEAD: usize = 384;

/// What the signed kind-445 event adds around the sealed content: `id`,
/// `pubkey` and `sig` hex, the `h` tag, `kind`/`created_at`, the JSON
/// punctuation, and the `["EVENT",{…}]` framing `RelayRuntime::publish`
/// counts. Same measurement, same guard.
const EVENT_FRAME_OVERHEAD: usize = 512;

/// **The wire cost of a kind-445 frame carrying `plaintext_len` bytes** — an
/// upper bound, never an estimate that could come in under.
///
/// This is the number a payload has to be judged against BEFORE it enters the
/// chain. `RelayRuntime::publish` refuses an over-budget event locally and
/// deterministically, and the outbox then holds its cursor at that envelope
/// on purpose (nothing recovers a skipped one) — so a payload that cannot be
/// framed inside the budget wedges everything the node writes after it,
/// across restarts. The honest place to refuse it is where a human can still
/// choose a smaller one, which is the propose path in molt-engine.
///
/// The content is base64, so it needs no JSON escaping: the cost is a
/// function of the plaintext LENGTH alone, whatever the payload holds.
/// Saturating throughout: an absurd length must report "far over budget",
/// never panic on overflow. The answer is the same either way — refuse.
#[must_use]
pub fn frame_cost(plaintext_len: usize) -> usize {
    let sealed = plaintext_len
        .saturating_add(MLS_FRAME_OVERHEAD)
        .saturating_add(NONCE_LEN)
        .saturating_add(TAG_LEN);
    // base64 of the sealed bytes, padded to a multiple of 4
    sealed
        .div_ceil(3)
        .saturating_mul(4)
        .saturating_add(EVENT_FRAME_OVERHEAD)
}

/// The inverse of [`frame_cost`]: the largest plaintext whose frame still
/// fits `budget` bytes. `0` when the budget cannot carry the framing at all.
///
/// Every step rounds DOWN, so `frame_cost(max_plaintext_for(b)) <= b` holds
/// by construction rather than by luck.
#[must_use]
pub fn max_plaintext_for(budget: u64) -> usize {
    let budget = usize::try_from(budget).unwrap_or(usize::MAX);
    let Some(sealed_b64) = budget.checked_sub(EVENT_FRAME_OVERHEAD) else {
        return 0;
    };
    let sealed = sealed_b64 / 4 * 3;
    sealed.saturating_sub(MLS_FRAME_OVERHEAD + NONCE_LEN + TAG_LEN)
}

/// The base64 alphabet of the 445 content (standard, padded — what every
/// Nostr client encodes with).
pub fn encode_base64(raw: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(raw)
}

/// Decode a 445 content's base64, as a shape error when it is not base64.
pub fn decode_base64(content: &str) -> Result<Vec<u8>, EnvelopeError> {
    base64::engine::general_purpose::STANDARD
        .decode(content)
        .map_err(|e| EnvelopeError::Shape(format!("not base64: {e}")))
}

/// The h-tag rotation window: **24 h, aligned to UTC day boundaries, the
/// SAME for every republic** (concept §4.4). Uniformity is the load-bearing
/// choice: a per-group window would make the rotation *cadence* a
/// fingerprint and each rotation a solo, timing-linkable event, while one
/// shared window rotates every group at once — all old tags go quiet and
/// all new ones appear together, so an observer gets a batch with no
/// old→new mapping. Only the timing is uniform; the tag VALUES stay
/// per-group secret.
pub const H_WINDOW: u64 = 86_400;

/// The group's `h` tag for the window containing `unix_secs`:
/// `SHA-256("molt-h-tag-v1\0" ‖ rotation_seed ‖ le64(window))`, lowercase
/// hex. Deterministic and announcement-free — every member derives the same
/// tag independently, and an offline member re-derives whatever it missed
/// ([`h_tags_for_catchup`]), so there is no rotation to miss and no grace
/// window to link.
///
/// The `rotation_seed` is a STABLE group secret set at founding and
/// delivered in the Welcome — never the epoch-rotating exporter secret.
pub fn h_tag(rotation_seed: &[u8; 32], unix_secs: u64) -> String {
    use sha2::Digest as _;
    let window = unix_secs / H_WINDOW;
    let mut hasher = sha2::Sha256::new_with_prefix(b"molt-h-tag-v1\0");
    hasher.update(rotation_seed);
    hasher.update(window.to_le_bytes());
    hex::encode(hasher.finalize())
}

/// Every `h` tag from the window of `since_secs` through the window of
/// `now_secs`, oldest first — what a returning member subscribes to so the
/// windows it slept through are not lost. Bounded by `max_windows` (newest
/// kept): a long-absent member asks a relay for a horizon, never for a year.
pub fn h_tags_for_catchup(
    rotation_seed: &[u8; 32],
    since_secs: u64,
    now_secs: u64,
    max_windows: usize,
) -> Vec<String> {
    let first = since_secs / H_WINDOW;
    let last = now_secs / H_WINDOW;
    let windows = last.saturating_sub(first).saturating_add(1);
    let take = u64::try_from(max_windows).unwrap_or(u64::MAX).min(windows);
    let start = last.saturating_sub(take.saturating_sub(1));
    (start..=last)
        .map(|w| h_tag(rotation_seed, w.saturating_mul(H_WINDOW)))
        .collect()
}

/// Why a kind-445 event's tag set is not acceptable. The list IS the
/// contract (`mdk_evaluation.md` §2.1): exactly one `h`, at most one
/// `expiration`, and **no other tag whatsoever** — a group event carries no
/// `p`, no `e`, nothing a relay or a peer could use to say more about it
/// than "this belongs to that group id".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// No `h` tag at all — the event names no group.
    #[error("no h tag")]
    MissingH,
    /// More than one `h`. Counting OCCURRENCES rather than taking the first
    /// match is the point: a first-match extractor lets a second tag say
    /// something different to a different reader.
    #[error("more than one h tag")]
    DuplicateH,
    /// `["h"]` with no value.
    #[error("h tag without a value")]
    ValuelessH,
    /// `["h", v, …]` — an `h` tag with more than its value.
    #[error("h tag with extra elements")]
    OversizedH,
    /// A `[]` tag.
    #[error("empty tag")]
    EmptyTag,
    /// A tag that is neither `h` nor `expiration`.
    #[error("unexpected tag: {0}")]
    UnknownTag(String),
    /// The `h` value is not exactly 64 lowercase hex characters.
    #[error("h value must be 64 lowercase hex characters")]
    BadHValue,
    /// More than one `expiration`.
    #[error("more than one expiration tag")]
    DuplicateExpiration,
    /// `expiration` is missing, negative, non-numeric, or does not fit.
    #[error("expiration must be one non-negative integer")]
    BadExpiration,
}

/// Validate a kind-445 event's tags and return `(h_value, expiration)`.
///
/// Strict by construction: occurrences are COUNTED, the `h` value must be
/// the canonical 64-lowercase-hex group id (an uppercase spelling is a
/// different string to a relay's index, so it is refused rather than
/// normalized), and any tag outside the two allowed ones rejects the whole
/// event. Shape is checked before anything is decrypted (the peeler's
/// order): a frame we refuse never reaches the AEAD.
pub fn parse_445_tags(tags: &[Vec<String>]) -> Result<(String, Option<u64>), TagError> {
    let mut h: Option<&String> = None;
    let mut expiration: Option<u64> = None;
    let mut h_seen = 0usize;
    let mut exp_seen = 0usize;
    for tag in tags {
        let name = tag.first().ok_or(TagError::EmptyTag)?;
        match name.as_str() {
            "h" => {
                h_seen += 1;
                if h_seen > 1 {
                    return Err(TagError::DuplicateH);
                }
                match tag.len() {
                    1 => return Err(TagError::ValuelessH),
                    2 => h = Some(&tag[1]),
                    _ => return Err(TagError::OversizedH),
                }
            }
            "expiration" => {
                exp_seen += 1;
                if exp_seen > 1 {
                    return Err(TagError::DuplicateExpiration);
                }
                let raw = tag.get(1).ok_or(TagError::BadExpiration)?;
                // digits only: `str::parse` would accept a leading `+`, and
                // a relay reading the plain integer would disagree with us
                if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) || tag.len() != 2 {
                    return Err(TagError::BadExpiration);
                }
                expiration = Some(raw.parse::<u64>().map_err(|_| TagError::BadExpiration)?);
            }
            other => return Err(TagError::UnknownTag(other.to_string())),
        }
    }
    let h = h.ok_or(TagError::MissingH)?;
    if h.len() != 64
        || !h
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(TagError::BadHValue);
    }
    Ok((h.clone(), expiration))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape is exactly `base64(nonce ‖ ciphertext ‖ tag)`: the
    /// overhead is 28 bytes, never a hidden header (§10.11's "33 bytes
    /// smaller than the NIP-44 form" rests on this).
    #[test]
    fn the_wire_shape_is_nonce_then_sealed_bytes() {
        let sealed = seal_outer(&[7u8; 32], b"0123456789").expect("seal");
        let raw = decode_base64(&sealed).expect("base64");
        assert_eq!(
            raw.len(),
            NONCE_LEN + 10 + TAG_LEN,
            "nonce + plaintext + tag, no framing of our own"
        );
        // an empty payload is still a valid sealing (nonce + bare tag)
        let empty = decode_base64(&seal_outer(&[7u8; 32], b"").expect("seal")).expect("b64");
        assert_eq!(empty.len(), NONCE_LEN + TAG_LEN);
    }

    /// A wrong key never opens a frame — the group-shared secret is the only
    /// thing standing between a relay and the MLS frames.
    #[test]
    fn a_foreign_secret_does_not_open_it() {
        let sealed = seal_outer(&[1u8; 32], b"ours").expect("seal");
        assert_eq!(
            open_outer(&[[2u8; 32], [3u8; 32]], &sealed),
            Err(EnvelopeError::EpochOpaque)
        );
        assert_eq!(open_outer(&[[1u8; 32]], &sealed).expect("ours opens"), b"ours");
    }
}
