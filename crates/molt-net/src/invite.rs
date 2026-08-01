// SPDX-License-Identifier: GPL-3.0-or-later

//! Founding-ritual invites (transport concept §3.3).
//!
//! A founding invite carries a **high-entropy single-use ticket** and the
//! transport path to the founder's invite queue. Activating it is bound to
//! the ticket by a MAC: the member sends
//! `JoinRequest{ name, identity pk, nostr pk, mac }` with
//! `mac = HMAC-SHA256(KDF(ticket), 0x02 ‖ name ‖ 0 ‖ pk ‖ 0 ‖ nostr_pk)`
//! (v2 — the explicit version byte keeps a v1 link from being replayed into
//! a v2 seat, and binding the Nostr key is what anchors the roster's third
//! anchor to the ticket holder). The founder verifies the MAC against the
//! unspent ticket and spends it — a bare leaked queue address cannot knock,
//! and a replayed ticket is rejected.
//!
//! The ritual's two wire messages (`JoinRequest` on the invite queue,
//! `SealSigned` on the reply queue) ride the transport as ordinary
//! payloads; here they are the plaintext inside the per-queue wrap. From
//! T2 on they sit inside MLS ciphertext.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::NetError;

type HmacSha256 = Hmac<Sha256>;

/// Ticket length in bytes (256-bit single-use secret).
const TICKET_LEN: usize = 32;

/// A fresh single-use invite ticket, lowercase hex (256-bit).
pub fn mint_ticket() -> Result<String, NetError> {
    let mut t = [0u8; TICKET_LEN];
    getrandom::getrandom(&mut t)
        .map_err(|e| NetError::Crypto(format!("os rng unavailable: {e}")))?;
    Ok(hex::encode(t))
}

/// The MAC binding an activation to its ticket (v2):
/// `HMAC-SHA256(KDF(ticket), 0x02 ‖ name ‖ 0 ‖ pk ‖ 0 ‖ nostr_pk)`,
/// lowercase hex. The leading version byte makes the layout explicit, so a
/// MAC minted under the v1 formula (`name ‖ 0 ‖ pk` — no version byte, no
/// Nostr anchor) can never verify against a v2 seat; binding `nostr_pk`
/// makes the roster's third anchor as ticket-bound as the identity key.
/// `KDF(ticket)` is a domain-separated SHA-256 of the ticket (unchanged
/// from v1), so the raw ticket is never the HMAC key directly.
pub fn join_mac(ticket: &str, name: &str, identity_pk: &str, nostr_pk: &str) -> String {
    hex::encode(
        join_mac_state(ticket, name, identity_pk, nostr_pk)
            .finalize()
            .into_bytes(),
    )
}

/// The keyed, fully-updated HMAC state behind [`join_mac`] and
/// [`verify_join_mac`] — ONE place computes the layout, so mint and verify
/// cannot drift.
fn join_mac_state(ticket: &str, name: &str, identity_pk: &str, nostr_pk: &str) -> HmacSha256 {
    let mut kdf = Sha256::new_with_prefix(b"molt-invite-mac-key\0");
    use sha2::Digest;
    kdf.update(ticket.as_bytes());
    let key = kdf.finalize();
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(&[0x02u8]);
    mac.update(name.as_bytes());
    mac.update(&[0u8]);
    mac.update(identity_pk.as_bytes());
    mac.update(&[0u8]);
    mac.update(nostr_pk.as_bytes());
    mac
}

/// Verify an activation MAC against the ticket in constant time. The
/// caller is still responsible for single-use (spend the ticket on the
/// first accepted request).
pub fn verify_join_mac(
    ticket: &str,
    name: &str,
    identity_pk: &str,
    nostr_pk: &str,
    mac_hex: &str,
) -> bool {
    // The wire form is exactly lowercase hex of a 32-byte HMAC — an
    // uppercase or malformed spelling is not the MAC we minted (this
    // shape check is over the ATTACKER'S input only, so its timing
    // reveals nothing about the expected value).
    if mac_hex.len() != 64
        || !mac_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Ok(given) = hex::decode(mac_hex) else {
        return false;
    };
    // the battle-tested constant-time compare (subtle, inside hmac) —
    // replaces the hand-rolled XOR fold (mdk_evaluation.md §7.7)
    join_mac_state(ticket, name, identity_pk, nostr_pk)
        .verify_slice(&given)
        .is_ok()
}

/// The **v2 invite handover** (N4a, `nostr_n4_plan.md` §3): what a founding
/// invite link carries beyond the display preview — everything a joining
/// node needs to reach the founder over Nostr. Replaces the queue-shaped
/// `server\nqueue_id\nwrap\nseat` blob of the pre-N4 link, which could not
/// even authenticate a join (it carried only a ticket prefix).
///
/// In memory the npub is the CANONICAL hex anchor (the roster form); on the
/// wire it travels as bech32 (`npub1…`, the link/UI form — concept §3). The
/// decode side is strict and fail-closed: a link is untrusted input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteHandoverV2 {
    /// The seat this invite fills (0-based).
    pub seat: u32,
    /// The FULL single-use ticket (64 lowercase hex) — the MAC key material.
    pub ticket: String,
    /// The founder's transport anchor, canonical x-only hex (the gift-wrap
    /// recipient for the JoinRequest).
    pub npub: String,
    /// The invite relays (normalized `ws://`/`wss://` URLs, 1..=
    /// [`crate::welcome::MAX_PAYLOAD_RELAYS`]).
    pub relays: Vec<String>,
}

const INVITE_HANDOVER_VERSION: u8 = 2;

/// The transport handover a **recovery** link carries (N4b).
///
/// The founding twin plus the republic id, minus the seat: a rejoiner is
/// returning to a seat it already holds, so there is no seat index to pick,
/// but it CANNOT derive the republic id (it has no roster yet) while the seat
/// proof must bind exactly that id — so the coordinator carries it.
///
/// Both sides then check it against their own: the coordinator verifies the
/// proof against ITS id, so a doctored link's id simply fails to verify, and
/// the rejoiner re-derives the real id from the genesis once it catches up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryHandoverV2 {
    /// The FULL single-use recovery ticket (64 lowercase hex).
    pub ticket: String,
    /// The COORDINATOR's transport anchor — the gift-wrap recipient for the
    /// RecoverRequest.
    pub npub: String,
    /// The relays the coordinator listens on.
    pub relays: Vec<String>,
    /// The republic's content-derived id, which the seat proof binds.
    pub republic_id: String,
}

const RECOVERY_HANDOVER_VERSION: u8 = 2;

#[derive(Serialize, Deserialize)]
struct RecoveryWire {
    v: u8,
    ticket: String,
    npub: String,
    relays: Vec<String>,
    republic_id: String,
}

impl RecoveryHandoverV2 {
    /// Render the handover as one URL-safe hex segment.
    pub fn encode(&self) -> Result<String, NetError> {
        use nostr::nips::nip19::ToBech32;
        if self.ticket.len() != 64 || !is_lower_hex(&self.ticket) {
            return Err(NetError::Framing("recovery ticket is malformed".into()));
        }
        if self.republic_id.is_empty() || !is_lower_hex(&self.republic_id) {
            return Err(NetError::Framing("recovery republic id is malformed".into()));
        }
        let relays = InviteHandoverV2::check_relays(&self.relays)?;
        let canonical = crate::nostr::canonical_nostr_pk(&self.npub)?;
        let npub = nostr::PublicKey::from_hex(&canonical)
            .map_err(|e| NetError::Framing(format!("npub encode: {e}")))?
            .to_bech32()
            .map_err(|e| NetError::Framing(format!("npub encode: {e}")))?;
        let wire = RecoveryWire {
            v: RECOVERY_HANDOVER_VERSION,
            ticket: self.ticket.clone(),
            npub,
            relays,
            republic_id: self.republic_id.clone(),
        };
        Ok(hex::encode(
            serde_json::to_string(&wire).map_err(|e| NetError::Framing(e.to_string()))?,
        ))
    }

    /// Parse and validate — strict, fail-closed, and honest about a
    /// pre-N4b (queue-shaped) recovery link.
    pub fn decode(blob: &str) -> Result<Self, NetError> {
        let bytes = hex::decode(blob.trim())
            .map_err(|_| NetError::Framing("not a recovery handover segment".into()))?;
        let wire = String::from_utf8(bytes)
            .map_err(|_| NetError::Framing("not a recovery handover segment".into()))?;
        let parsed: RecoveryWire = serde_json::from_str(&wire).map_err(|_| {
            if wire.contains('\n') {
                NetError::Framing(
                    "this is a queue-shaped recovery link from an older build — \
                     ask for a fresh recovery link on this build"
                        .into(),
                )
            } else {
                NetError::Framing("not a recovery handover".into())
            }
        })?;
        if parsed.v != RECOVERY_HANDOVER_VERSION {
            return Err(NetError::Framing(format!(
                "unsupported recovery handover version {} — this build reads \
                 v{RECOVERY_HANDOVER_VERSION}",
                parsed.v
            )));
        }
        if parsed.ticket.len() != 64 || !is_lower_hex(&parsed.ticket) {
            return Err(NetError::Framing("recovery ticket is malformed".into()));
        }
        if parsed.republic_id.is_empty() || !is_lower_hex(&parsed.republic_id) {
            return Err(NetError::Framing("recovery republic id is malformed".into()));
        }
        use nostr::nips::nip19::FromBech32;
        let pk = nostr::PublicKey::from_bech32(&parsed.npub)
            .map_err(|e| NetError::Framing(format!("recovery npub: {e}")))?;
        // bech32 decoding does no curve validation — the ONE anchor gate does
        let npub = crate::nostr::canonical_nostr_pk(&pk.to_hex())?;
        let relays = InviteHandoverV2::check_relays(&parsed.relays)?;
        Ok(RecoveryHandoverV2 {
            ticket: parsed.ticket,
            npub,
            relays,
            republic_id: parsed.republic_id,
        })
    }
}

/// The wire form: versioned JSON, npub as bech32.
#[derive(Serialize, Deserialize)]
struct HandoverWire {
    v: u8,
    seat: u32,
    ticket: String,
    npub: String,
    relays: Vec<String>,
}

fn is_lower_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl InviteHandoverV2 {
    /// The pre-hex wire JSON — split from [`encode`](Self::encode) so tests
    /// can tamper with individual fields.
    fn wire(&self) -> Result<String, NetError> {
        use nostr::nips::nip19::ToBech32;
        let canonical = crate::nostr::canonical_nostr_pk(&self.npub)?;
        let npub = nostr::PublicKey::from_hex(&canonical)
            .map_err(|e| NetError::Framing(format!("npub encode: {e}")))?
            .to_bech32()
            .map_err(|e| NetError::Framing(format!("npub encode: {e}")))?;
        let wire = HandoverWire {
            v: INVITE_HANDOVER_VERSION,
            seat: self.seat,
            ticket: self.ticket.clone(),
            npub,
            relays: self.relays.clone(),
        };
        serde_json::to_string(&wire).map_err(|e| NetError::Framing(e.to_string()))
    }

    /// Render the handover as one URL-safe hex segment.
    pub fn encode(&self) -> Result<String, NetError> {
        if self.ticket.len() != 64 || !is_lower_hex(&self.ticket) {
            return Err(NetError::Framing("invite ticket is malformed".into()));
        }
        Self::check_relays(&self.relays)?;
        Ok(hex::encode(self.wire()?))
    }

    /// Parse and validate a handover segment — strict, fail-closed, and
    /// honest about a pre-N4 (queue-shaped) link.
    pub fn decode(blob: &str) -> Result<Self, NetError> {
        let bytes = hex::decode(blob.trim())
            .map_err(|_| NetError::Framing("not an invite handover segment".into()))?;
        let wire = String::from_utf8(bytes)
            .map_err(|_| NetError::Framing("not an invite handover segment".into()))?;
        let parsed: HandoverWire = serde_json::from_str(&wire).map_err(|_| {
            if wire.contains('\n') {
                NetError::Framing(
                    "this is a queue-shaped invite from an older build — \
                     mint a fresh invite on this build"
                        .into(),
                )
            } else {
                NetError::Framing("not an invite handover".into())
            }
        })?;
        if parsed.v != INVITE_HANDOVER_VERSION {
            return Err(NetError::Framing(format!(
                "unsupported invite handover version {} — this build reads v{INVITE_HANDOVER_VERSION}",
                parsed.v
            )));
        }
        if parsed.ticket.len() != 64 || !is_lower_hex(&parsed.ticket) {
            return Err(NetError::Framing("invite ticket is malformed".into()));
        }
        use nostr::nips::nip19::FromBech32;
        let pk = nostr::PublicKey::from_bech32(&parsed.npub)
            .map_err(|e| NetError::Framing(format!("invite npub: {e}")))?;
        // bech32 decoding does no curve validation either — canonicalize
        // through the ONE anchor gate (normalize-or-reject)
        let npub = crate::nostr::canonical_nostr_pk(&pk.to_hex())?;
        let relays = Self::check_relays(&parsed.relays)?;
        Ok(InviteHandoverV2 {
            seat: parsed.seat,
            ticket: parsed.ticket,
            npub,
            relays,
        })
    }

    /// 1..=MAX relays, each normalized through the WHATWG-parser gate.
    fn check_relays(relays: &[String]) -> Result<Vec<String>, NetError> {
        if relays.is_empty() {
            return Err(NetError::Framing("an invite must carry at least one relay".into()));
        }
        if relays.len() > crate::welcome::MAX_PAYLOAD_RELAYS {
            return Err(NetError::Framing(format!(
                "{} relays — more than the {} an invite may carry",
                relays.len(),
                crate::welcome::MAX_PAYLOAD_RELAYS
            )));
        }
        relays
            .iter()
            .map(|r| {
                molt_core::relay::normalize_relay_url(r)
                    .map_err(|e| NetError::Framing(format!("invite relay {r:?}: {e}")))
            })
            .collect()
    }
}

/// Where the founder sends the canonical table back: the reply queue the
/// joining member created and subscribed to. In SMP each party owns the
/// queue it *receives* on, so the reply queue belongs to the member and its
/// address travels here, inside the `JoinRequest`. All fields are strings so
/// the handover needs no serde on the transport address types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyHandover {
    /// The reply queue's server (`smp://fingerprint@host`; the loopback hub
    /// ignores it).
    pub server: String,
    /// The reply queue's send-side id, lowercase hex.
    pub queue_id: String,
    /// The reply queue's per-queue wrap key, lowercase hex.
    pub wrap: String,
}

/// One member's activation of a founding invite (transport concept §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// Which invite this answers (0-based seat index).
    pub seat: u32,
    /// The member's self-chosen display name.
    pub name: String,
    /// The member's per-workspace identity public key, lowercase hex.
    pub identity_pk: String,
    /// The member's Nostr transport anchor (x-only BIP-340 key, lowercase
    /// hex), ticket-salted per seat (`molt_net::nostr_identity`). Additive:
    /// empty only from a pre-N1 sender — whose MAC then fails v2 anyway.
    #[serde(default)]
    pub nostr_pk: String,
    /// `join_mac(ticket, name, identity_pk, nostr_pk)` (v2).
    pub mac: String,
    /// The member's reply queue, so the founder can send the table back.
    /// `None` only on the legacy path where the founder pre-created it.
    #[serde(default)]
    pub reply: Option<ReplyHandover>,
    /// The member's MLS KeyPackage (hex of the wire bytes), so the founder can
    /// add it to the group and the same identity anchors both the genesis table
    /// and the MLS credential (concept §3.3). Empty on a pre-MLS path.
    #[serde(default)]
    pub key_package: String,
}

/// A total-loss member's activation of a recovery link (recovery_ritual.md §4):
/// it proves its seat with a signature by its RE-DERIVED identity key (the seat
/// proof), so a leaked link alone cannot answer the challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverRequest {
    /// The seat's member handle.
    pub member: String,
    /// The member's re-derived identity pk (must equal the anchored one), hex.
    pub identity_pk: String,
    /// The member's fresh MLS KeyPackage (hex of the wire bytes) to re-key its leaf.
    pub key_package: String,
    /// The single-use recovery ticket the seat proof is bound to.
    pub ticket: String,
    /// The seat proof: `sign(identity, ticket ‖ key_package ‖ republic_id ‖
    /// new_nostr_pk)` under `molt-seat-proof-v2`, hex.
    pub seat_proof: String,
    /// The rejoiner's NEW transport anchor (N4b §8.3). The founding anchor is
    /// ticket-salted and cannot be re-derived once the ticket is gone, so a
    /// recovered seat brings a fresh key — carried here, bound by the seat
    /// proof, and made authoritative by riding the threshold-signed
    /// `Restored` block. Empty on the loopback path, which has no transport
    /// anchor at all.
    #[serde(default)]
    pub new_nostr_pk: String,
    /// The member's reply queue, so the coordinator can send the Welcome back.
    #[serde(default)]
    pub reply: Option<ReplyHandover>,
}

/// One member's seal signature over the final roster table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealSigned {
    /// Which invite this answers (0-based seat index).
    pub seat: u32,
    /// Ed25519 signature over `roster_canonical_bytes`, lowercase hex.
    pub sig: String,
}

/// The founding-ritual wire vocabulary. Founder→member carries the final
/// canonical table to sign; member→founder carries the join request then
/// the seal signature. One tagged enum so a queue can carry either leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RitualMsg {
    /// member → founder: activate the invite link.
    Join(JoinRequest),
    /// returning member → coordinator: activate a recovery link (prove the seat
    /// with a re-derived-identity signature — recovery_ritual.md §4).
    Recover(RecoverRequest),
    /// founder → member: the proposed constitution to ratify — JSON of the
    /// transport's pre-attestation `SealedRoster` (name, republic id, m/n,
    /// roster, identities, agenda; attestations empty). The member **recomputes**
    /// the canonical table from these fields (so what it signs provably matches
    /// the name + agenda + roster it is shown), checks its own (name, key) is
    /// present, and — on the human's confirm — signs. Signing IS the
    /// ratification (concept §3.3). Opaque here — this layer keeps no core types.
    Seal {
        /// JSON of the pre-attestation sealed roster (the proposal).
        proposal: String,
    },
    /// founder → member: the founder **accepted** this activation — the ticket
    /// verified, the identity + KeyPackage are anchored, the seat is filled. Sent
    /// right after anchoring so the joiner gets immediate feedback ("you're in,
    /// waiting for the deliberation") instead of a silent wait until the charter.
    /// Advisory only: it carries no authority and the joiner still verifies the
    /// eventual `Seal`/`Genesis`.
    JoinAccepted {
        /// Which invite this answers (0-based seat index).
        seat: u32,
    },
    /// founder → member: this activation was REJECTED because the invite's
    /// single-use ticket was already spent by another member (the founder
    /// sent the same link twice by mistake). Sent on the second joiner's
    /// advertised reply queue so they fail fast with the reason instead of
    /// waiting forever; they need their own, unused link. Additive: an older
    /// joiner simply fails to parse it and keeps its (old) silent wait.
    LinkSpent {
        /// Which invite this answers (0-based seat index).
        seat: u32,
        /// WHY, in the founder's words. The founder distinguishes three very
        /// different situations that all end in this frame — a second person
        /// on one link, a retry after the group already formed, and a seat
        /// displaced by its own owner's re-activation — and only the first is
        /// fixed by asking for a fresh link. Additive: an older peer decodes
        /// it as absent and falls back to its old wording.
        #[serde(default)]
        reason: String,
    },
    /// founder → member: this founding is OVER — the founder cancelled, or it
    /// failed on the founder's side. Sent so a member stops waiting instead
    /// of sitting in an unbounded wait forever: before the group is born it
    /// travels as a gift-wrap to each anchored seat, after birth as a 445
    /// group frame.
    ///
    /// **Never authoritative for anything persisted** — it only ends a run.
    /// Additive: an older peer fails to parse it and keeps its old silent
    /// wait, exactly like `LinkSpent`.
    Aborted {
        /// Why, in the founder's words — shown to the member.
        reason: String,
    },
    /// member → founder: the signature over the table.
    Signed(SealSigned),
    /// member → founder: the member explicitly **declined** the proposed
    /// charter (as opposed to silently going away). Lets the founder mark the
    /// seat and decide (cancel + re-mint) instead of waiting forever.
    Declined {
        /// Which invite this answers (0-based seat index).
        seat: u32,
    },
    /// Either direction, after sealing: a node's runtime-mesh handover
    /// announcement, carried as **MLS ciphertext** (hex) over the founding star
    /// (T2 mesh bootstrap). The founder relays members' announcements to the
    /// other members; each node decrypts to the authenticated sender.
    MeshAnnounce {
        /// Hex of the MLS-encrypted `molt_net::mesh::MeshAnnounce`.
        ct: String,
    },
    /// founder → member: the complete sealed roster (JSON of the transport's
    /// `SealedRoster`), sent once every seat has signed, so the member writes
    /// its own genesis. Opaque here — this layer keeps no core types. Carries
    /// the MLS Welcome (hex) alongside it, so the member joins the group and
    /// finishes the ritual already inside it (concept §3.3).
    Genesis {
        /// JSON of the sealed roster (identities + all attestations + the
        /// republic id).
        sealed: String,
        /// The MLS Welcome for this founding (hex of the wire bytes). Empty on
        /// a pre-MLS path.
        #[serde(default)]
        welcome: String,
    },
    /// coordinator → returning member: the MLS **Welcome** that re-admits a
    /// recovered seat (`recovery_ritual.md` §4 ❺–❻), sent on the rejoiner's reply
    /// queue once the `Membership{Restored}` block commits and the coordinator
    /// re-keys the group. It also carries the **persistent chain** (genesis →
    /// head) so the rejoiner catches its whole state up over this same recovery
    /// channel (recovery option A) — verified from block 0, no live mesh needed.
    Welcome {
        /// The MLS Welcome for the re-key (hex of the wire bytes).
        welcome: String,
        /// The full persistent chain as JSON of `Vec<ChainBlock>` — the
        /// coordinator's `chain.state`, for the rejoiner to verify + materialize.
        /// Opaque here (this layer keeps no core types). Empty on a chain-less
        /// republic.
        #[serde(default)]
        chain: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- invite handover v2 (N4a step 3, nostr_n4_plan.md §3) -------------

    /// bech32-encode an arbitrary x coordinate — `PublicKey::from_hex` does
    /// no curve validation (the N1 lesson), which is exactly what lets the
    /// test mint an npub for a non-point.
    fn nostr_bech32_for_test(hex_x: &str) -> String {
        use nostr::nips::nip19::ToBech32;
        nostr::PublicKey::from_hex(hex_x)
            .expect("from_hex is unvalidated")
            .to_bech32()
            .expect("bech32")
    }

    fn replace_npub(wire: &str, npub: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(wire).expect("json");
        v["npub"] = serde_json::Value::String(npub.to_string());
        serde_json::to_string(&v).expect("json")
    }

    fn handover() -> InviteHandoverV2 {
        InviteHandoverV2 {
            seat: 2,
            ticket: "ab".repeat(32),
            // a REAL x-only key — the decode side validates the curve point
            npub: nostr::Keys::generate().public_key().to_hex(),
            relays: vec!["wss://relay.example".to_string()],
        }
    }

    /// KEYSTONE — the v2 handover round-trips through one URL-safe hex
    /// segment; the npub travels as bech32 (the link/UI form, concept §3)
    /// and comes back as the canonical hex anchor.
    #[test]
    fn the_v2_handover_round_trips_with_a_bech32_npub_on_the_wire() {
        let h = handover();
        let blob = h.encode().expect("encode");
        assert!(
            blob.chars().all(|c| c.is_ascii_hexdigit()),
            "one URL-safe hex segment: {blob}"
        );
        let wire = String::from_utf8(hex::decode(&blob).expect("hex")).expect("utf8");
        assert!(wire.contains("npub1"), "bech32 in the link form: {wire}");
        assert!(!wire.contains(&h.npub), "never raw hex on the wire form");
        assert_eq!(InviteHandoverV2::decode(&blob).expect("decode"), h);
    }

    /// KEYSTONE — a v1 (queue-shaped) handover blob is refused with an
    /// error naming the older build; strict rejections for every malformed
    /// field: bad version, bad ticket shape, invalid npub (not a curve
    /// point), no relays, too many relays, an oversized/unnormalizable
    /// relay URL.
    #[test]
    fn the_v2_handover_decode_is_fail_closed() {
        // a genuine v1 blob: hex("server\nqueue\nwrap\nseat")
        let v1 = hex::encode("smp://x\naa\nbb\n2");
        let err = InviteHandoverV2::decode(&v1).expect_err("v1 must refuse");
        assert!(
            err.to_string().contains("older build"),
            "the v1 rejection names the reason: {err}"
        );

        let cases: Vec<(&str, String)> = vec![
            ("wrong version", {
                let mut w = handover().wire().expect("wire");
                w = w.replace("\"v\":2", "\"v\":3");
                w
            }),
            ("short ticket", {
                let mut h = handover();
                h.ticket = "abcd".to_string();
                h.wire().expect("wire")
            }),
            ("uppercase ticket", {
                let mut h = handover();
                h.ticket = "AB".repeat(32);
                h.wire().expect("wire")
            }),
            ("npub that is not a point", {
                // x = p-1 is not on the curve for even y in general; use an
                // obviously-invalid all-ff x
                let w = handover().wire().expect("wire");
                let bad = nostr_bech32_for_test(&"ff".repeat(32));
                replace_npub(&w, &bad)
            }),
            ("no relays", {
                let mut h = handover();
                h.relays.clear();
                h.wire().expect("wire")
            }),
            ("too many relays", {
                let mut h = handover();
                h.relays = (0..9).map(|i| format!("wss://r{i}.example")).collect();
                h.wire().expect("wire")
            }),
            ("unnormalizable relay", {
                let mut h = handover();
                h.relays = vec!["https://not-a-ws.example".to_string()];
                h.wire().expect("wire")
            }),
        ];
        for (what, wire) in cases {
            let blob = hex::encode(&wire);
            assert!(
                InviteHandoverV2::decode(&blob).is_err(),
                "{what} must refuse: {wire}"
            );
        }
    }

    /// The npub in a decoded handover is CANONICAL: bech32 for a valid
    /// x-only key decodes to the same lowercase-hex form
    /// `canonical_nostr_pk` mints — one key, one spelling, roster-wide.
    #[test]
    fn a_decoded_npub_is_the_canonical_anchor_form() {
        let h = handover();
        let blob = h.encode().expect("encode");
        let back = InviteHandoverV2::decode(&blob).expect("decode");
        assert_eq!(
            back.npub,
            crate::nostr::canonical_nostr_pk(&h.npub).expect("canonical"),
            "decode yields the canonical hex anchor"
        );
    }

    #[test]
    fn a_recover_request_round_trips_tagged() {
        let msg = RitualMsg::Recover(RecoverRequest {
            member: "walter".to_string(),
            identity_pk: "aa".to_string(),
            key_package: "bb".to_string(),
            ticket: "cc".to_string(),
            seat_proof: "dd".to_string(),
            new_nostr_pk: String::new(),
            reply: Some(ReplyHandover {
                server: "smp://f@h".to_string(),
                queue_id: "ee".to_string(),
                wrap: "ff".to_string(),
            }),
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"kind\":\"recover\""), "tagged as recover: {json}");
        assert_eq!(
            serde_json::from_str::<RitualMsg>(&json).expect("deserialize"),
            msg
        );
    }

    /// N1 PIN — invite MAC v2 binds the third anchor with an explicit
    /// version byte: `HMAC-SHA256(KDF(ticket), 0x02 ‖ name ‖ 0 ‖ identity_pk
    /// ‖ 0 ‖ nostr_pk)`. Fixture computed INDEPENDENTLY (python hmac). The
    /// legacy v1 formula (no version byte, no nostr anchor) must NOT verify —
    /// a v1 link cannot be replayed into a v2 seat.
    #[test]
    fn join_mac_v2_binds_the_nostr_anchor_and_rejects_v1() {
        let idpk = "aa".repeat(32);
        let npk = "cc".repeat(32);
        let mac = join_mac("deadbeef", "ada", &idpk, &npk);
        assert_eq!(
            mac, "bf2327b8aa78c7aabd037cd5dba20b5411f95acdd304f4c8f1a37ab59ddebc30",
            "independently computed v2 fixture"
        );
        assert!(verify_join_mac("deadbeef", "ada", &idpk, &npk, &mac));
        // the wire form is EXACTLY lowercase hex: a re-encoded spelling of
        // the same value is refused (pins the strictness across the
        // verify_slice refactor), as is anything not 64 hex chars
        assert!(!verify_join_mac("deadbeef", "ada", &idpk, &npk, &mac.to_uppercase()));
        assert!(!verify_join_mac("deadbeef", "ada", &idpk, &npk, &mac[..62]));
        assert!(!verify_join_mac("deadbeef", "ada", &idpk, &npk, "zz"));
        // every bound field matters — the nostr anchor included
        assert!(!verify_join_mac("deadbeef", "ada", &idpk, &"ee".repeat(32), &mac));
        assert!(!verify_join_mac("deadbeef", "eva", &idpk, &npk, &mac));
        assert!(!verify_join_mac("beefdead", "ada", &idpk, &npk, &mac));
        // the v1-formula MAC over the same ticket/name/identity_pk (computed
        // with the pre-N1 layout) is rejected
        assert!(!verify_join_mac(
            "deadbeef",
            "ada",
            &idpk,
            &npk,
            "19426eda32c712fc7e1b5d8ee2409ba80168aeb5bd09133298237c2f094e64a1"
        ));
    }

    #[test]
    fn tickets_are_fresh_and_hex() {
        let a = mint_ticket().expect("mint");
        assert_eq!(a.len(), TICKET_LEN * 2);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, mint_ticket().expect("mint2"));
    }

    #[test]
    fn ritual_msg_roundtrips_each_leg() {
        for msg in [
            RitualMsg::Join(JoinRequest {
                seat: 2,
                name: "juno".into(),
                identity_pk: "aa".repeat(32),
                nostr_pk: "ee".repeat(32),
                mac: "bb".repeat(32),
                reply: None,
                key_package: "cc".repeat(20),
            }),
            RitualMsg::Seal {
                proposal: "{\"name\":\"Guild\"}".into(),
            },
            RitualMsg::Signed(SealSigned {
                seat: 2,
                sig: "dd".repeat(64),
            }),
        ] {
            let wire = serde_json::to_vec(&msg).expect("encode");
            let back: RitualMsg = serde_json::from_slice(&wire).expect("decode");
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn mac_binds_ticket_name_and_key() {
        let t = mint_ticket().expect("mint");
        let mac = join_mac(&t, "petra", "aa", "cc");
        assert!(verify_join_mac(&t, "petra", "aa", "cc", &mac));
        // wrong ticket, name or key all fail
        assert!(!verify_join_mac(&mint_ticket().expect("m"), "petra", "aa", "cc", &mac));
        assert!(!verify_join_mac(&t, "walter", "aa", "cc", &mac));
        assert!(!verify_join_mac(&t, "petra", "bb", "cc", &mac));
        // no boundary confusion between name and pk, or pk and nostr pk
        assert_ne!(join_mac(&t, "petraa", "a", "cc"), join_mac(&t, "petra", "aa", "cc"));
        assert_ne!(join_mac(&t, "petra", "aac", "c"), join_mac(&t, "petra", "aa", "cc"));
        // garbage / wrong-length mac is rejected, never panics
        assert!(!verify_join_mac(&t, "petra", "aa", "cc", "deadbeef"));
        assert!(!verify_join_mac(&t, "petra", "aa", "cc", ""));
    }
}
