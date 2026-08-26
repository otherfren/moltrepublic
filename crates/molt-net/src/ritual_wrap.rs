// SPDX-License-Identifier: GPL-3.0-or-later

//! N4a (`docs_archive/transport/nostr_n4_plan.md` §2): the kind-446 **ritual
//! rumor** — every founder↔joiner pre-group ritual leg (JoinRequest,
//! JoinAccepted, LinkSpent) carries the existing [`RitualMsg`] JSON
//! vocabulary verbatim inside a NIP-59 gift wrap. Kind 446 is ours (no
//! interop, concept §10.3); 444 stays the Welcome, 445 the group event.
//!
//! Same posture as [`crate::welcome`]: ephemeral outer author, UNSIGNED
//! inner rumor, fail-closed peel. And the property the founder's ingest
//! leans on (§2.1): NIP-59's peel verifies the seal signature AND refuses a
//! rumor whose claimed author differs from the sealer — so the sender this
//! module returns is **proof of possession** of that nostr key, which is
//! what upgrades the roster's third anchor from "chosen and bound" to
//! "proven possessed" at join time.

use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey};

use crate::invite::RitualMsg;
use crate::welcome::GIFT_PLAINTEXT_MAX;

/// Our gift-wrapped ritual-message rumor kind.
pub use crate::kinds::KIND_RITUAL;

/// What went wrong wrapping or peeling a ritual message.
#[derive(Debug, thiserror::Error)]
pub enum RitualWrapError {
    /// The gift wrap could not be built (RNG / signing).
    #[error("wrapping the ritual message: {0}")]
    Wrap(String),
    /// The message does not fit the NIP-44 plaintext cap.
    #[error("ritual message is {bytes} bytes - over the {cap}-byte gift-wrap cap")]
    TooLarge {
        /// Measured encoded size.
        bytes: usize,
        /// The cap it exceeds ([`GIFT_PLAINTEXT_MAX`]).
        cap: usize,
    },
    /// Not addressed to us — indistinguishable from corrupt, on purpose.
    #[error("not addressed to this key")]
    NotForUs,
    /// The gift opened, but its rumor is not a kind-446 ritual message.
    #[error("inner event is kind {kind}, not a {KIND_RITUAL} ritual message")]
    NotARitual {
        /// The kind actually found inside.
        kind: u16,
    },
    /// The rumor is kind 446, but its content is not the RitualMsg
    /// vocabulary.
    #[error("ritual payload: {0}")]
    Payload(String),
}

/// Gift-wrap a [`RitualMsg`] to `recipient`, authored as `sender` inside
/// the seal. Returns the publishable kind-1059 event.
pub async fn wrap_ritual(
    sender: &Keys,
    recipient: &PublicKey,
    msg: &RitualMsg,
) -> Result<Event, RitualWrapError> {
    let content =
        serde_json::to_string(msg).map_err(|e| RitualWrapError::Payload(e.to_string()))?;
    if content.len() > GIFT_PLAINTEXT_MAX {
        return Err(RitualWrapError::TooLarge {
            bytes: content.len(),
            cap: GIFT_PLAINTEXT_MAX,
        });
    }
    let rumor = EventBuilder::new(Kind::Custom(KIND_RITUAL), content).build(sender.public_key());
    EventBuilder::gift_wrap(sender, recipient, rumor, [])
        .await
        .map_err(|e| RitualWrapError::Wrap(e.to_string()))
}

/// Peel a gift-wrapped ritual message addressed to `us`. Returns the
/// message and the PROVEN sender (seal author == rumor author, enforced by
/// the NIP-59 unwrap) — every step fail-closed.
pub async fn peel_ritual(
    us: &Keys,
    wrap: &Event,
) -> Result<(RitualMsg, PublicKey), RitualWrapError> {
    let unwrapped = UnwrappedGift::from_gift_wrap(us, wrap)
        .await
        .map_err(|_| RitualWrapError::NotForUs)?;
    let rumor = unwrapped.rumor;
    let kind = rumor.kind.as_u16();
    if kind != KIND_RITUAL {
        return Err(RitualWrapError::NotARitual { kind });
    }
    let msg: RitualMsg = serde_json::from_str(&rumor.content)
        .map_err(|e| RitualWrapError::Payload(format!("not a ritual message: {e}")))?;
    Ok((msg, rumor.pubkey))
}
