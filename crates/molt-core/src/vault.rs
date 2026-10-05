// SPDX-License-Identifier: GPL-3.0-or-later

//! The vault's shared vocabulary (`docs/vault/vault_threshold_disclosure.md`):
//! the chain records, the read-model view and the redacting text types.
//! Types only; the canonical byte layouts join in S1.

use serde::{Deserialize, Serialize};

use crate::MemberId;

/// D4: the plaintext cap, bytes.
pub const VAULT_PAYLOAD_MAX: usize = 102_400;
/// The longest deposit name, chars.
pub const VAULT_NAME_MAX: usize = 64;
/// The longest deposit kind, chars.
pub const VAULT_KIND_MAX: usize = 24;

/// A deposit's plaintext. `Debug` prints only its length: the text must
/// never reach a log (plan 1.3.17).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretText(pub String);

impl std::fmt::Debug for SecretText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

/// Hex of secret material on the wire (a share, an ephemeral ikm, an
/// answer ciphertext). `Debug` prints only its length.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretHex(pub String);

impl std::fmt::Debug for SecretHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} hex>", self.0.len())
    }
}

/// Raw secret bytes at rest (the vault seed). `Debug` prints only the
/// length; wiped on drop.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(transparent)]
pub struct SecretBytes(pub Vec<u8>);

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0.len())
    }
}

/// The vault context of ONE chain, built from its genesis roster or its
/// anchor's `founding_identities` - never from node state (plan 1.3.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultCtx {
    /// The threshold.
    pub m: u8,
    /// `(name, identity_pk, vault_pk)` in genesis founding-table order;
    /// a seat's Shamir x is its 1-based position here.
    pub holders_in_genesis_order: Vec<(MemberId, String, String)>,
}

/// Why a vault command was refused (`MoltError::Vault`). `Display` is the
/// English/MCP text; the GUI localizes per variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VaultRefusal {
    /// The founding bounds (spec §3).
    #[error("needs 2 <= m <= n-2")]
    Bounds,
    /// `set_features` cannot add the vault.
    #[error("founding only")]
    FoundingOnly,
    /// A joiner without vault support (D14).
    #[error("needs a newer version: {0}")]
    NeedsNewerVersion(MemberId),
    /// The deposit failed this seat's check.
    #[error("not verified")]
    NotVerified,
    /// The payload file is not here.
    #[error("payload not held")]
    PayloadNotHeld,
    /// Only the granted reader reads.
    #[error("not the reader")]
    NotTheReader,
    /// Not a vault republic.
    #[error("no vault")]
    NoVault,
    /// This seat lacks its vault seed (plan 1.3.2).
    #[error("no vault key")]
    NoVaultKey,
    /// The generic `propose` on the vault.
    #[error("use vault_seal")]
    UseVaultSeal,
    /// Over [`VAULT_PAYLOAD_MAX`].
    #[error("too large")]
    TooLarge,
    /// Outside the closed op set (plan 1.3.9).
    #[error("unknown op")]
    UnknownOp,
}

/// Where a deposit's ciphertext lives on the file plane.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultPayloadRef {
    /// SHA-256 of the payload ciphertext, lowercase hex.
    pub hash: String,
    /// Ciphertext length, bytes.
    pub size: u64,
}

/// A deposit record (spec §6). Hex strings; field order = canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultDeposit {
    /// The depositing seat.
    pub depositor: MemberId,
    /// The deposit's name; with `depositor` the last-wins slot.
    pub name: String,
    /// What it is (`text`, `password`, ...), visible to all (D8).
    pub kind: String,
    /// The threshold it was dealt at.
    pub m: u8,
    /// Every seat but the depositor, in genesis founding-table order.
    pub holders: Vec<String>,
    /// Feldman commitments, one per coefficient.
    pub commitments: Vec<String>,
    /// One HPKE-sealed share per holder, in `holders` order.
    pub enc_share: Vec<String>,
    /// The payload file.
    pub payload: VaultPayloadRef,
    /// 16 random bytes, fresh per deposit (plan 1.3.3).
    pub nonce: String,
    /// The depositor's identity signature over the canonical record.
    pub sig_depositor: String,
}

/// A grant record (spec §6): one reader for one version.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultGrant {
    /// Content-derived id (`molt-vault-grant-v1`).
    pub grant_id: String,
    /// The version it releases.
    pub secret_id: String,
    /// The one seat that may read.
    pub reader: MemberId,
}

/// An `Applied` payload on `Surface::Vault` (contract 2.1). `VaultBase`
/// is legal only as the fold entry of a vault cut (plan 1.3.9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum VaultOp {
    /// A deposit or replace.
    Deposit(VaultDeposit),
    /// A grant.
    Grant(VaultGrant),
    /// The folded base.
    VaultBase(VaultPayloadRef),
}

/// A holder's verdict on a deposit, as last reported (status, never
/// consensus).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultReceipt {
    /// `true` = share and payload verified, `false` = complaint.
    pub verified: bool,
    /// Revision; last wins.
    pub rev: u64,
}

/// The decided outcome of a complaint reveal (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultRevealOutcome {
    /// The reveal does not recompute the record: the depositor lied.
    Lie,
    /// The dealt share fails Feldman: the depositor dealt badly.
    BadShare,
    /// The share was fine: the complainer is named.
    FalseComplaint,
}

/// Receipts and reveal outcomes, last-wins per holder, per `secret_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultStatusStore {
    /// `secret_id` -> holder -> receipt.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub receipts: std::collections::BTreeMap<String, std::collections::BTreeMap<MemberId, VaultReceipt>>,
    /// `secret_id` -> holder -> decided reveal.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub reveals:
        std::collections::BTreeMap<String, std::collections::BTreeMap<MemberId, VaultRevealOutcome>>,
}

impl VaultStatusStore {
    /// Nothing recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty() && self.reveals.is_empty()
    }
}

/// A grant this seat applied that a reorg displaced (plan 1.4, Q1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultDisplacedGrant {
    /// The grant's content id.
    pub grant_id: String,
    /// The deposit name it released.
    pub name: String,
    /// The reader it named.
    pub reader: MemberId,
}

/// Validate a deposit name: trimmed, non-empty, at most
/// [`VAULT_NAME_MAX`] chars, no control characters.
pub fn check_vault_name(name: &str) -> Result<(), String> {
    check_label(name, VAULT_NAME_MAX, "name")
}

/// Validate a deposit kind like [`check_vault_name`], at most
/// [`VAULT_KIND_MAX`] chars.
pub fn check_vault_kind(kind: &str) -> Result<(), String> {
    check_label(kind, VAULT_KIND_MAX, "kind")
}

fn check_label(s: &str, max: usize, what: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err(format!("{what} is empty"));
    }
    if s.trim() != s {
        return Err(format!("{what} has outer spaces"));
    }
    if s.chars().count() > max {
        return Err(format!("{what} over {max} chars"));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("{what} has control chars"));
    }
    Ok(())
}

/// How much of the committed vault base is here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultBaseProgress {
    /// Bytes held so far.
    pub have: u64,
    /// Bytes the commitment names.
    pub size: u64,
}

/// A deposit card's state (spec §7: what is proven, not what committed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultDepositState {
    /// Its proposal is open.
    #[default]
    Pending,
    /// In the chain; fewer than m holders verified.
    Committed,
    /// m holders other than the depositor verified.
    Sealed,
    /// All n-1 holders verified.
    Hardened,
}

/// A complaint's status on a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultComplaintStatus {
    /// No reveal yet.
    #[default]
    Open,
    /// The depositor dealt a bad share.
    BadShare,
    /// The complaint was false.
    False,
    /// The depositor's reveal lied.
    Lie,
}

/// This seat's own check of a deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultMyCheck {
    /// Share and payload verified.
    Ok,
    /// Something failed.
    Bad,
    /// Not a holder (the depositor).
    #[default]
    None,
    /// Not checked yet.
    Pending,
}

/// A grant card's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultGrantState {
    /// Its proposal is open.
    #[default]
    Pending,
    /// In the chain, on the current version.
    Committed,
    /// In the chain, on a replaced version: nobody answers.
    Void,
    /// Applied here, then displaced by a reorg (plan 1.4).
    Displaced,
}

/// One complaint line on a deposit card.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultComplaintView {
    /// The complaining holder.
    pub holder: MemberId,
    /// Where it stands.
    pub status: VaultComplaintStatus,
}

/// One deposit card.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultDepositView {
    /// The version's content id.
    pub secret_id: String,
    /// The depositing seat.
    pub depositor: MemberId,
    /// Visible name (D8).
    pub name: String,
    /// Visible kind (D8).
    pub kind: String,
    /// Payload ciphertext size, bytes.
    pub size: u64,
    /// Card state.
    pub state: VaultDepositState,
    /// The open proposal, while pending.
    pub proposal: Option<u64>,
    /// The `secret_id` this version replaces.
    pub replaces: Option<String>,
    /// Holders with a `verified` receipt and no open complaint.
    pub verified: u8,
    /// n-1.
    pub holders: u8,
    /// `max(0, m - valid revealed shares)`.
    pub readable_by: u8,
    /// Complaint lines.
    pub complaints: Vec<VaultComplaintView>,
    /// Mine, and a valid reveal exists: offer a re-seal.
    pub reseal: bool,
    /// Deposited by this seat.
    pub mine: bool,
    /// The payload file is held here.
    pub held: bool,
    /// This seat's own check.
    pub my_check: VaultMyCheck,
}

/// One grant card (pending vote or audit entry).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultGrantView {
    /// Content id.
    pub grant_id: String,
    /// The version it releases.
    pub secret_id: String,
    /// That version's depositor.
    pub depositor: MemberId,
    /// That version's name.
    pub name: String,
    /// The elected reader.
    pub reader: MemberId,
    /// Card state.
    pub state: VaultGrantState,
    /// The open proposal, while pending.
    pub proposal: Option<u64>,
    /// Local commit time (unix seconds) when the log has it.
    pub at: Option<u64>,
    /// This seat is the reader.
    pub mine: bool,
    /// Valid answers collected this session (reader side).
    pub answers: u8,
    /// Shares needed.
    pub need: u8,
    /// Seats whose answer failed Feldman.
    pub bad_answers: Vec<MemberId>,
}

/// The vault surface's read model (`SurfaceSnapshot.vault`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultView {
    /// The genesis is roster-v6: a real vault, not the mock.
    pub real: bool,
    /// Threshold.
    pub m: u8,
    /// Seats.
    pub n: u8,
    /// The folded base still arriving.
    pub base_pending: Option<VaultBaseProgress>,
    /// Deposit cards.
    pub deposits: Vec<VaultDepositView>,
    /// Grant cards.
    pub grants: Vec<VaultGrantView>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The UI's contract: a filled view serializes to exactly this shape.
    #[test]
    fn vault_view_json_shape_is_pinned() {
        let view = VaultView {
            real: true,
            m: 2,
            n: 4,
            base_pending: Some(VaultBaseProgress { have: 1, size: 2 }),
            deposits: vec![VaultDepositView {
                secret_id: "s1".to_string(),
                depositor: "a".to_string(),
                name: "one".to_string(),
                kind: "text".to_string(),
                size: 3,
                state: VaultDepositState::Sealed,
                proposal: Some(7),
                replaces: Some("s0".to_string()),
                verified: 2,
                holders: 3,
                readable_by: 1,
                complaints: vec![
                    VaultComplaintView { holder: "b".to_string(), status: VaultComplaintStatus::Open },
                    VaultComplaintView { holder: "c".to_string(), status: VaultComplaintStatus::BadShare },
                    VaultComplaintView { holder: "d".to_string(), status: VaultComplaintStatus::False },
                    VaultComplaintView { holder: "b".to_string(), status: VaultComplaintStatus::Lie },
                ],
                reseal: true,
                mine: true,
                held: true,
                my_check: VaultMyCheck::None,
            }],
            grants: vec![VaultGrantView {
                grant_id: "g1".to_string(),
                secret_id: "s1".to_string(),
                depositor: "a".to_string(),
                name: "one".to_string(),
                reader: "b".to_string(),
                state: VaultGrantState::Displaced,
                proposal: None,
                at: Some(9),
                mine: false,
                answers: 1,
                need: 2,
                bad_answers: vec!["c".to_string()],
            }],
        };
        let want = json!({
            "real": true,
            "m": 2,
            "n": 4,
            "base_pending": { "have": 1, "size": 2 },
            "deposits": [{
                "secret_id": "s1",
                "depositor": "a",
                "name": "one",
                "kind": "text",
                "size": 3,
                "state": "sealed",
                "proposal": 7,
                "replaces": "s0",
                "verified": 2,
                "holders": 3,
                "readable_by": 1,
                "complaints": [
                    { "holder": "b", "status": "open" },
                    { "holder": "c", "status": "bad_share" },
                    { "holder": "d", "status": "false" },
                    { "holder": "b", "status": "lie" }
                ],
                "reseal": true,
                "mine": true,
                "held": true,
                "my_check": "none"
            }],
            "grants": [{
                "grant_id": "g1",
                "secret_id": "s1",
                "depositor": "a",
                "name": "one",
                "reader": "b",
                "state": "displaced",
                "proposal": null,
                "at": 9,
                "mine": false,
                "answers": 1,
                "need": 2,
                "bad_answers": ["c"]
            }]
        });
        assert_eq!(serde_json::to_value(&view).expect("serializes"), want);
        let back: VaultView = serde_json::from_value(want).expect("deserializes");
        assert_eq!(back, view);
        // the remaining enum words
        for (state, word) in [
            (VaultDepositState::Pending, "pending"),
            (VaultDepositState::Committed, "committed"),
            (VaultDepositState::Hardened, "hardened"),
        ] {
            assert_eq!(serde_json::to_value(state).expect("serializes"), json!(word));
        }
        for (check, word) in [
            (VaultMyCheck::Ok, "ok"),
            (VaultMyCheck::Bad, "bad"),
            (VaultMyCheck::Pending, "pending"),
        ] {
            assert_eq!(serde_json::to_value(check).expect("serializes"), json!(word));
        }
        for (state, word) in [
            (VaultGrantState::Pending, "pending"),
            (VaultGrantState::Committed, "committed"),
            (VaultGrantState::Void, "void"),
        ] {
            assert_eq!(serde_json::to_value(state).expect("serializes"), json!(word));
        }
    }

    #[test]
    fn secret_text_debug_never_prints_the_text() {
        let text = SecretText("hunter2-password".to_string());
        let shown = format!("{text:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "<16 bytes>");
        // nested in a command, too
        let cmd = crate::Command::VaultSeal {
            name: "a".to_string(),
            kind: "text".to_string(),
            text: text.clone(),
        };
        assert!(!format!("{cmd:?}").contains("hunter2"));
        // the wire carries the plain string
        assert_eq!(serde_json::to_value(&text).expect("serializes"), json!("hunter2-password"));
        let hex = SecretHex("abcdef".to_string());
        assert_eq!(format!("{hex:?}"), "<6 hex>");
    }

    #[test]
    fn secret_bytes_debug_never_prints_the_bytes_and_serializes_as_a_vec() {
        let seed = SecretBytes(vec![0xab; 32]);
        assert_eq!(format!("{seed:?}"), "<32 bytes>");
        let wire = serde_json::to_value(&seed).expect("serializes");
        assert_eq!(wire, serde_json::to_value(vec![0xab_u8; 32]).expect("serializes"));
        let back: SecretBytes = serde_json::from_value(wire).expect("deserializes");
        assert_eq!(back, seed);
        let ts = crate::TransportState { vault_seed: Some(seed), ..Default::default() };
        let shown = format!("{ts:?}");
        assert!(!shown.contains("171"), "{shown}");
        assert!(shown.contains("<32 bytes>"), "{shown}");
    }

    /// Contract 2.1: the Applied op shapes on `Surface::Vault`, flattened.
    #[test]
    fn vault_op_json_shape_is_pinned() {
        let payload = VaultPayloadRef { hash: "h".to_string(), size: 3 };
        let dep = VaultOp::Deposit(VaultDeposit {
            depositor: "a".to_string(),
            name: "one".to_string(),
            kind: "text".to_string(),
            m: 2,
            holders: vec!["b".to_string()],
            commitments: vec!["c0".to_string()],
            enc_share: vec!["e0".to_string()],
            payload: payload.clone(),
            nonce: "n".to_string(),
            sig_depositor: "s".to_string(),
        });
        let grant = VaultOp::Grant(VaultGrant {
            grant_id: "g".to_string(),
            secret_id: "x".to_string(),
            reader: "b".to_string(),
        });
        let base = VaultOp::VaultBase(payload);
        let cases = [
            (
                dep,
                json!({
                    "op": "deposit", "depositor": "a", "name": "one", "kind": "text", "m": 2,
                    "holders": ["b"], "commitments": ["c0"], "enc_share": ["e0"],
                    "payload": { "hash": "h", "size": 3 }, "nonce": "n", "sig_depositor": "s"
                }),
            ),
            (grant, json!({ "op": "grant", "grant_id": "g", "secret_id": "x", "reader": "b" })),
            (base, json!({ "op": "vault_base", "hash": "h", "size": 3 })),
        ];
        for (op, want) in cases {
            assert_eq!(serde_json::to_value(&op).expect("serializes"), want);
            let back: VaultOp = serde_json::from_value(want).expect("deserializes");
            assert_eq!(back, op);
        }
        // the closed set (plan 1.3.9): nothing else decodes
        for other in [
            json!({ "op": "seal_secret", "title": "a" }),
            json!({ "op": "unknown" }),
            json!({ "hash": "h", "size": 3 }),
        ] {
            assert!(serde_json::from_value::<VaultOp>(other).is_err());
        }
    }

    #[test]
    fn vault_labels_are_checked_at_their_edges() {
        type Check = fn(&str) -> Result<(), String>;
        let cases: [(Check, usize); 2] =
            [(check_vault_name, VAULT_NAME_MAX), (check_vault_kind, VAULT_KIND_MAX)];
        for (check, max) in cases {
            assert!(check("").is_err());
            assert!(check("   ").is_err());
            assert!(check(" a").is_err());
            assert!(check("a ").is_err());
            assert!(check("a b").is_ok());
            assert!(check(&"ä".repeat(max)).is_ok(), "{max} multibyte chars");
            assert!(check(&"ä".repeat(max + 1)).is_err());
            assert!(check("a\nb").is_err());
            assert!(check("a\u{7f}b").is_err());
        }
    }

    /// Contract 2.1: the English (MCP) text of every refusal.
    #[test]
    fn vault_refusal_text_is_pinned() {
        let cases = [
            (VaultRefusal::Bounds, "needs 2 <= m <= n-2"),
            (VaultRefusal::FoundingOnly, "founding only"),
            (VaultRefusal::NeedsNewerVersion("b".to_string()), "needs a newer version: b"),
            (VaultRefusal::NotVerified, "not verified"),
            (VaultRefusal::PayloadNotHeld, "payload not held"),
            (VaultRefusal::NotTheReader, "not the reader"),
            (VaultRefusal::NoVault, "no vault"),
            (VaultRefusal::NoVaultKey, "no vault key"),
            (VaultRefusal::UseVaultSeal, "use vault_seal"),
            (VaultRefusal::TooLarge, "too large"),
            (VaultRefusal::UnknownOp, "unknown op"),
        ];
        for (r, want) in cases {
            assert_eq!(r.to_string(), want);
        }
        assert_eq!(
            crate::MoltError::Vault(VaultRefusal::NoVaultKey).to_string(),
            "vault: no vault key"
        );
    }
}
