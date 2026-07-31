// SPDX-License-Identifier: GPL-3.0-or-later

//! N3 (`docs/transport/nostr_n3_plan.md` §4): the kind-444 **Welcome** —
//! the MLS Welcome carried to one invitee, NIP-59 gift-wrapped to its
//! transport anchor.
//!
//! Two deliberate properties, both from NIP-EE:
//!
//! * The outer event (kind 1059) is authored by a **fresh ephemeral key**,
//!   so a relay sees an anonymous gift addressed to one pubkey — never the
//!   founder's anchor, never a link between two invitees.
//! * The inner rumor is **UNSIGNED**: a leaked 444 is not publishable by
//!   whoever finds it. Authorship still authenticates — the rumor's
//!   `pubkey` is the inviter's anchor, and only the holder of the
//!   recipient's secret can reach it at all.
//!
//! The peel chain is fail-closed: wrong recipient, wrong inner kind, or a
//! payload that is not the agreed encoding all REFUSE rather than
//! half-interpret. We dropped kind 443, so unlike Marmot's
//! `wrap_welcome_with_metadata` there is no KeyPackage event id to bind
//! (`mdk_evaluation.md` §2.1, adaptation 3).

use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey};

/// The NIP-EE Welcome kind.
pub const KIND_WELCOME: u16 = 444;

/// What went wrong wrapping or peeling a Welcome.
#[derive(Debug, thiserror::Error)]
pub enum WelcomeError {
    /// The gift wrap could not be built (RNG / signing).
    #[error("wrapping the welcome: {0}")]
    Wrap(String),
    /// This gift is not addressed to us — we cannot open it. Indistinguishable
    /// from a corrupt gift on purpose: a recipient learns nothing about
    /// traffic that is not theirs.
    #[error("not addressed to this key")]
    NotForUs,
    /// The gift opened, but its rumor is not a kind-444 Welcome.
    #[error("inner event is kind {kind}, not a {KIND_WELCOME} welcome")]
    NotAWelcome {
        /// The kind actually found inside.
        kind: u16,
    },
    /// The rumor is a Welcome, but its content is not the agreed encoding.
    #[error("welcome payload: {0}")]
    Payload(String),
}

/// Gift-wrap `welcome` (the MLS Welcome bytes) to `invitee`, authored as
/// `inviter` inside the seal. Returns the publishable kind-1059 event.
pub async fn wrap_welcome(
    inviter: &Keys,
    invitee: &PublicKey,
    welcome: &[u8],
) -> Result<Event, WelcomeError> {
    // hex, not base64: the 444 payload is small and hex keeps the rumor
    // trivially inspectable in a debug dump without a decoder
    let rumor = EventBuilder::new(Kind::Custom(KIND_WELCOME), hex::encode(welcome))
        .build(inviter.public_key());
    EventBuilder::gift_wrap(inviter, invitee, rumor, [])
        .await
        .map_err(|e| WelcomeError::Wrap(e.to_string()))
}

/// Peel a gift-wrapped Welcome addressed to `us`. Returns the MLS Welcome
/// bytes and the INVITER's anchor (the rumor's author) — every step
/// fail-closed.
pub async fn peel_welcome(us: &Keys, wrap: &Event) -> Result<(Vec<u8>, PublicKey), WelcomeError> {
    let unwrapped = UnwrappedGift::from_gift_wrap(us, wrap)
        .await
        .map_err(|_| WelcomeError::NotForUs)?;
    let rumor = unwrapped.rumor;
    let kind = rumor.kind.as_u16();
    if kind != KIND_WELCOME {
        return Err(WelcomeError::NotAWelcome { kind });
    }
    let bytes = hex::decode(rumor.content.as_bytes())
        .map_err(|e| WelcomeError::Payload(format!("not hex: {e}")))?;
    if bytes.is_empty() {
        return Err(WelcomeError::Payload("empty welcome".into()));
    }
    Ok((bytes, rumor.pubkey))
}
