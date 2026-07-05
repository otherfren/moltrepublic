// SPDX-License-Identifier: GPL-3.0-or-later

//! Founding-ritual invites (transport concept §3.3).
//!
//! A founding invite carries a **high-entropy single-use ticket** and the
//! transport path to the founder's invite queue. Activating it is bound to
//! the ticket by a MAC: the member sends
//! `JoinRequest{ name, identity pk, mac }` with
//! `mac = HMAC-SHA256(KDF(ticket), name ‖ pk)`. The founder verifies the
//! MAC against the unspent ticket and spends it — a bare leaked queue
//! address cannot knock, and a replayed ticket is rejected.
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

/// The MAC binding an activation to its ticket:
/// `HMAC-SHA256(KDF(ticket), name ‖ 0 ‖ pk)`, lowercase hex. `KDF(ticket)`
/// is a domain-separated SHA-256 of the ticket, so the raw ticket is never
/// the HMAC key directly.
pub fn join_mac(ticket: &str, name: &str, identity_pk: &str) -> String {
    let mut kdf = Sha256::new_with_prefix(b"molt-invite-mac-key\0");
    use sha2::Digest;
    kdf.update(ticket.as_bytes());
    let key = kdf.finalize();
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(name.as_bytes());
    mac.update(&[0u8]);
    mac.update(identity_pk.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify an activation MAC against the ticket in constant time. The
/// caller is still responsible for single-use (spend the ticket on the
/// first accepted request).
pub fn verify_join_mac(ticket: &str, name: &str, identity_pk: &str, mac_hex: &str) -> bool {
    let expected = join_mac(ticket, name, identity_pk);
    // hex of a fixed-size HMAC: constant-time compare of equal-length
    // strings; unequal length is an immediate reject
    let a = expected.as_bytes();
    let b = mac_hex.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
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
    /// `join_mac(ticket, name, identity_pk)`.
    pub mac: String,
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
    /// founder → member: the final `roster_canonical_bytes` to sign
    /// (hex).
    Seal {
        /// The canonical table bytes, lowercase hex.
        table: String,
    },
    /// member → founder: the signature over the table.
    Signed(SealSigned),
}

#[cfg(test)]
mod tests {
    use super::*;

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
                mac: "bb".repeat(32),
            }),
            RitualMsg::Seal {
                table: "cc".repeat(40),
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
        let mac = join_mac(&t, "petra", "aa");
        assert!(verify_join_mac(&t, "petra", "aa", &mac));
        // wrong ticket, name or key all fail
        assert!(!verify_join_mac(&mint_ticket().expect("m"), "petra", "aa", &mac));
        assert!(!verify_join_mac(&t, "walter", "aa", &mac));
        assert!(!verify_join_mac(&t, "petra", "bb", &mac));
        // no boundary confusion between name and pk
        assert_ne!(join_mac(&t, "petraa", "a"), join_mac(&t, "petra", "aa"));
        // garbage / wrong-length mac is rejected, never panics
        assert!(!verify_join_mac(&t, "petra", "aa", "deadbeef"));
        assert!(!verify_join_mac(&t, "petra", "aa", ""));
    }
}
