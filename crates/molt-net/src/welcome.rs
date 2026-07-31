// SPDX-License-Identifier: GPL-3.0-or-later

//! N3 (`docs/transport/nostr_n3_plan.md` §4) + N4a (`nostr_n4_plan.md` §4):
//! the kind-444 **Welcome** — the MLS Welcome carried to one invitee,
//! NIP-59 gift-wrapped to its transport anchor. Since N4 the rumor content
//! is the versioned **payload v2**: beside the MLS Welcome it carries the
//! group's `rotation_seed` and relay list, the two things a joiner (and a
//! total-loss rejoiner) can only learn "inside the authenticated Welcome,
//! never before" (concept §4.2 finding 9).
//!
//! Two deliberate properties, both from NIP-EE:
//!
//! * The outer event (kind 1059) is authored by a **fresh ephemeral key**,
//!   so a relay sees an anonymous gift addressed to one pubkey — never the
//!   founder's anchor, never a link between two invitees.
//! * The inner rumor is **UNSIGNED**: a leaked 444 is not publishable by
//!   whoever finds it. Authorship still authenticates — NIP-59's peel
//!   verifies the seal signature AND that the rumor's claimed author equals
//!   the sealer, so the returned sender is proven, not asserted.
//!
//! The peel chain is fail-closed: wrong recipient, wrong inner kind, or a
//! payload that is not the agreed v2 encoding all REFUSE rather than
//! half-interpret. We dropped kind 443, so unlike Marmot's
//! `wrap_welcome_with_metadata` there is no KeyPackage event id to bind
//! (`mdk_evaluation.md` §2.1, adaptation 3); the seed/relays ride the
//! payload instead of a GroupContext extension (decision recorded in
//! `nostr_n4_plan.md` §4).

use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey};
use serde::{Deserialize, Serialize};

/// The NIP-EE Welcome kind.
pub const KIND_WELCOME: u16 = 444;

/// rust-nostr refuses NIP-44 plaintexts above this (its documented deviation
/// from the 65535-byte spec cap, pinned by the N0 canary in
/// `tests/nostr_vectors.rs`). Anything we gift-wrap must fit UNDER it, and
/// the wrap helpers measure BEFORE encrypting so an oversized payload fails
/// with a real message instead of an opaque encrypt error.
pub const GIFT_PLAINTEXT_MAX: usize = 65_408;

/// The most relay URLs a Welcome payload (or an invite link) may carry —
/// both are untrusted input at the receiving end, so the cap is enforced on
/// parse as well as on build.
pub const MAX_PAYLOAD_RELAYS: usize = 8;

/// What went wrong wrapping or peeling a Welcome.
#[derive(Debug, thiserror::Error)]
pub enum WelcomeError {
    /// The gift wrap could not be built (RNG / signing).
    #[error("wrapping the welcome: {0}")]
    Wrap(String),
    /// The payload does not fit the NIP-44 plaintext cap — a founding this
    /// size cannot ride a gift wrap, and the caller must surface that.
    #[error("welcome payload is {bytes} bytes — over the {cap}-byte gift-wrap cap")]
    TooLarge {
        /// Measured encoded payload size.
        bytes: usize,
        /// The cap it exceeds ([`GIFT_PLAINTEXT_MAX`]).
        cap: usize,
    },
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

/// What a Welcome delivers beside the MLS Welcome itself: the group secrets
/// a joiner cannot know before it — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomePayload {
    /// The MLS Welcome bytes (`MlsMember::add_members` /
    /// `restore_member` output).
    pub welcome: Vec<u8>,
    /// The group's stable h-tag seed (`envelope::h_tag`).
    pub rotation_seed: [u8; 32],
    /// The group's relay list (normalized URLs, at most
    /// [`MAX_PAYLOAD_RELAYS`]).
    pub relays: Vec<String>,
}

/// The wire form of [`WelcomePayload`] — versioned, hex-encoded byte fields.
#[derive(Serialize, Deserialize)]
struct WelcomeWire {
    v: u8,
    welcome: String,
    rotation_seed: String,
    relays: Vec<String>,
}

const WELCOME_PAYLOAD_VERSION: u8 = 2;

impl WelcomePayload {
    fn encode(&self) -> Result<String, WelcomeError> {
        if self.welcome.is_empty() {
            return Err(WelcomeError::Payload("empty welcome".into()));
        }
        if self.relays.len() > MAX_PAYLOAD_RELAYS {
            return Err(WelcomeError::Payload(format!(
                "{} relays — more than the {MAX_PAYLOAD_RELAYS} the payload may carry",
                self.relays.len()
            )));
        }
        let wire = WelcomeWire {
            v: WELCOME_PAYLOAD_VERSION,
            welcome: hex::encode(&self.welcome),
            rotation_seed: hex::encode(self.rotation_seed),
            relays: self.relays.clone(),
        };
        serde_json::to_string(&wire).map_err(|e| WelcomeError::Payload(e.to_string()))
    }

    fn decode(content: &str) -> Result<Self, WelcomeError> {
        let wire: WelcomeWire = serde_json::from_str(content)
            .map_err(|e| WelcomeError::Payload(format!("not a versioned welcome: {e}")))?;
        if wire.v != WELCOME_PAYLOAD_VERSION {
            return Err(WelcomeError::Payload(format!(
                "unsupported welcome payload version {}",
                wire.v
            )));
        }
        let welcome = hex::decode(wire.welcome.as_bytes())
            .map_err(|e| WelcomeError::Payload(format!("welcome not hex: {e}")))?;
        if welcome.is_empty() {
            return Err(WelcomeError::Payload("empty welcome".into()));
        }
        let seed_bytes = hex::decode(wire.rotation_seed.as_bytes())
            .map_err(|e| WelcomeError::Payload(format!("rotation_seed not hex: {e}")))?;
        let rotation_seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|_| WelcomeError::Payload("rotation_seed is not 32 bytes".into()))?;
        if wire.relays.len() > MAX_PAYLOAD_RELAYS {
            return Err(WelcomeError::Payload(format!(
                "{} relays — more than the {MAX_PAYLOAD_RELAYS} the payload may carry",
                wire.relays.len()
            )));
        }
        Ok(WelcomePayload {
            welcome,
            rotation_seed,
            relays: wire.relays,
        })
    }
}

/// Gift-wrap the Welcome payload to `invitee`, authored as `inviter` inside
/// the seal. Returns the publishable kind-1059 event. Refuses (never
/// truncates) a payload over [`GIFT_PLAINTEXT_MAX`].
pub async fn wrap_welcome(
    inviter: &Keys,
    invitee: &PublicKey,
    payload: &WelcomePayload,
) -> Result<Event, WelcomeError> {
    let content = payload.encode()?;
    if content.len() > GIFT_PLAINTEXT_MAX {
        return Err(WelcomeError::TooLarge {
            bytes: content.len(),
            cap: GIFT_PLAINTEXT_MAX,
        });
    }
    let rumor = EventBuilder::new(Kind::Custom(KIND_WELCOME), content).build(inviter.public_key());
    EventBuilder::gift_wrap(inviter, invitee, rumor, [])
        .await
        .map_err(|e| WelcomeError::Wrap(e.to_string()))
}

/// Peel a gift-wrapped Welcome addressed to `us`. Returns the payload and
/// the INVITER's anchor (the PROVEN seal/rumor author — NIP-59 verifies the
/// seal signature and the author match) — every step fail-closed.
pub async fn peel_welcome(
    us: &Keys,
    wrap: &Event,
) -> Result<(WelcomePayload, PublicKey), WelcomeError> {
    let unwrapped = UnwrappedGift::from_gift_wrap(us, wrap)
        .await
        .map_err(|_| WelcomeError::NotForUs)?;
    let rumor = unwrapped.rumor;
    let kind = rumor.kind.as_u16();
    if kind != KIND_WELCOME {
        return Err(WelcomeError::NotAWelcome { kind });
    }
    let payload = WelcomePayload::decode(&rumor.content)?;
    Ok((payload, rumor.pubkey))
}
