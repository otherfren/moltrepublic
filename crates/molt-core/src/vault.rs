// SPDX-License-Identifier: GPL-3.0-or-later

//! The vault's shared vocabulary (`docs_archive/vault/vault_threshold_disclosure.md`):
//! the chain records, the read-model view, the redacting text types and
//! the canonical byte layouts every vault primitive hashes, signs or binds.

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
/// answer ciphertext). `Debug` prints only its length; wiped on drop.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
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
    /// `set_features` cannot add the vault to a roster-v5 republic.
    #[error("needs a newer republic")]
    NeedsNewerRepublic,
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
    /// A deposit whose `replaces` is not the slot's current version.
    #[error("stale version")]
    Stale,
}

/// Whether a `set_features` vote can switch this republic's vault on
/// (spec §3): `StatusView.vault_enable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultEnable {
    /// The vault is in the effective features.
    On,
    /// A roster-v6 republic within the bounds: votable.
    Offer,
    /// A roster-v5 genesis carries no vault keys.
    #[default]
    NeedsNewerRepublic,
    /// Outside `2 <= m <= n-2`.
    Bounds,
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
    /// The `secret_id` this version replaces, empty for a first deposit;
    /// signed, so a replayed older record cannot roll the slot back.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub replaces: String,
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
    /// `secret_id` -> holders whose real share a reveal published, lie or not.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub valid_reveals: std::collections::BTreeMap<String, std::collections::BTreeSet<MemberId>>,
}

impl VaultStatusStore {
    /// Nothing recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty() && self.reveals.is_empty() && self.valid_reveals.is_empty()
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

// ---------------------------------------------------------------------
// Canonical layouts (spec §5 encoding rule): NUL-terminated tag, then the
// field count, then every field le32-length-prefixed; a list field is
// itself a counted run. Every tag belongs to exactly one primitive.
// ---------------------------------------------------------------------

/// The vault key derivation info (plan 1.3.1).
pub const TAG_KEY: &str = "molt-vault-x25519-v1";
/// [`secret_id`].
pub const TAG_SECRET: &str = "molt-vault-secret-v1";
/// [`deposit_signing_bytes`].
pub const TAG_DEPOSIT: &str = "molt-vault-deposit-v1";
/// [`grant_id`].
pub const TAG_GRANT: &str = "molt-vault-grant-v1";
/// [`share_aad`].
pub const TAG_SHARE: &str = "molt-vault-share-v1";
/// [`resp_aad`].
pub const TAG_RESP: &str = "molt-vault-resp-v1";
/// [`payload_aad`].
pub const TAG_PAYLOAD: &str = "molt-vault-payload-v1";
/// [`dek_info`].
pub const TAG_DEK: &str = "molt-vault-dek-v1";
/// [`eph_info`].
pub const TAG_EPH: &str = "molt-vault-eph-v1";
/// [`deal_info`].
pub const TAG_DEAL: &str = "molt-vault-deal-v1";
/// [`secret_scalar_info`].
pub const TAG_SECRET_SCALAR: &str = "molt-vault-secret-scalar-v1";
/// [`vault_base_canonical_bytes`].
pub const TAG_BASE: &str = "molt-vault-base-v1";
/// [`payload_series_info`].
pub const TAG_PAYLOAD_SERIES: &str = "molt-vault-payload-series-v1";
/// [`base_series_info`].
pub const TAG_BASE_SERIES: &str = "molt-vault-base-series-v1";

/// Every vault tag; pinned duplicate-free.
pub const VAULT_TAGS: [&str; 14] = [
    TAG_KEY,
    TAG_SECRET,
    TAG_DEPOSIT,
    TAG_GRANT,
    TAG_SHARE,
    TAG_RESP,
    TAG_PAYLOAD,
    TAG_DEK,
    TAG_EPH,
    TAG_DEAL,
    TAG_SECRET_SCALAR,
    TAG_BASE,
    TAG_PAYLOAD_SERIES,
    TAG_BASE_SERIES,
];

enum Field<'a> {
    One(&'a [u8]),
    Run(Vec<&'a [u8]>),
}

fn layout(tag: &str, fields: &[Field<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(tag.as_bytes());
    out.push(0);
    crate::put_count(&mut out, fields.len());
    for f in fields {
        match f {
            Field::One(b) => crate::put_bytes(&mut out, b),
            Field::Run(items) => {
                crate::put_count(&mut out, items.len());
                for i in items {
                    crate::put_bytes(&mut out, i);
                }
            }
        }
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn strs(v: &[String]) -> Vec<&[u8]> {
    v.iter().map(String::as_bytes).collect()
}

/// HKDF info of the vault key: `founding_nostr_pk ‖ identity_pk` (plan 1.3.1).
#[must_use]
pub fn vault_key_info(founding_nostr_pk: &str, identity_pk: &str) -> Vec<u8> {
    layout(TAG_KEY, &[Field::One(founding_nostr_pk.as_bytes()), Field::One(identity_pk.as_bytes())])
}

/// The preimage of [`secret_id`]: every field but `enc_share`, `nonce`,
/// the signature and the payload size (spec §6).
#[must_use]
pub fn secret_id_bytes(republic_id: &str, dep: &VaultDeposit) -> Vec<u8> {
    let m = [dep.m];
    layout(
        TAG_SECRET,
        &[
            Field::One(republic_id.as_bytes()),
            Field::One(dep.depositor.as_bytes()),
            Field::One(dep.name.as_bytes()),
            Field::One(dep.kind.as_bytes()),
            Field::One(dep.replaces.as_bytes()),
            Field::One(&m),
            Field::Run(strs(&dep.holders)),
            Field::Run(strs(&dep.commitments)),
            Field::One(dep.payload.hash.as_bytes()),
        ],
    )
}

/// A deposit version's content id, lowercase hex.
#[must_use]
pub fn secret_id(republic_id: &str, dep: &VaultDeposit) -> String {
    sha256_hex(&secret_id_bytes(republic_id, dep))
}

/// What `sig_depositor` signs: every field but the signature itself.
#[must_use]
pub fn deposit_signing_bytes(republic_id: &str, dep: &VaultDeposit) -> Vec<u8> {
    let m = [dep.m];
    let size = dep.payload.size.to_le_bytes();
    layout(
        TAG_DEPOSIT,
        &[
            Field::One(republic_id.as_bytes()),
            Field::One(dep.depositor.as_bytes()),
            Field::One(dep.name.as_bytes()),
            Field::One(dep.kind.as_bytes()),
            Field::One(dep.replaces.as_bytes()),
            Field::One(&m),
            Field::Run(strs(&dep.holders)),
            Field::Run(strs(&dep.commitments)),
            Field::Run(strs(&dep.enc_share)),
            Field::One(dep.payload.hash.as_bytes()),
            Field::One(&size),
            Field::One(dep.nonce.as_bytes()),
        ],
    )
}

/// The preimage of [`grant_id`].
#[must_use]
pub fn grant_id_bytes(secret_id: &str, reader: &str, proposal_id: u64) -> Vec<u8> {
    let id = proposal_id.to_le_bytes();
    layout(
        TAG_GRANT,
        &[Field::One(secret_id.as_bytes()), Field::One(reader.as_bytes()), Field::One(&id)],
    )
}

/// A grant's content id, lowercase hex: survives a cut that drops heights.
#[must_use]
pub fn grant_id(secret_id: &str, reader: &str, proposal_id: u64) -> String {
    sha256_hex(&grant_id_bytes(secret_id, reader, proposal_id))
}

/// HPKE AAD of a holder's dealt share.
#[must_use]
pub fn share_aad(secret_id: &str, seat: &str) -> Vec<u8> {
    layout(TAG_SHARE, &[Field::One(secret_id.as_bytes()), Field::One(seat.as_bytes())])
}

/// HPKE AAD of a holder's answer to a grant.
#[must_use]
pub fn resp_aad(republic_id: &str, grant_id: &str, seat: &str) -> Vec<u8> {
    layout(
        TAG_RESP,
        &[Field::One(republic_id.as_bytes()), Field::One(grant_id.as_bytes()), Field::One(seat.as_bytes())],
    )
}

fn slot(tag: &str, republic_id: &str, depositor: &str, name: &str, kind: &str) -> Vec<u8> {
    layout(
        tag,
        &[
            Field::One(republic_id.as_bytes()),
            Field::One(depositor.as_bytes()),
            Field::One(name.as_bytes()),
            Field::One(kind.as_bytes()),
        ],
    )
}

/// AEAD AAD of the payload file.
#[must_use]
pub fn payload_aad(republic_id: &str, depositor: &str, name: &str, kind: &str) -> Vec<u8> {
    slot(TAG_PAYLOAD, republic_id, depositor, name, kind)
}

/// HKDF info of the payload key derived from the shared scalar.
#[must_use]
pub fn dek_info(republic_id: &str, depositor: &str, name: &str, kind: &str) -> Vec<u8> {
    slot(TAG_DEK, republic_id, depositor, name, kind)
}

/// HKDF info of a holder's deterministic HPKE ephemeral.
#[must_use]
pub fn eph_info(secret_id: &str, seat: &str) -> Vec<u8> {
    layout(TAG_EPH, &[Field::One(secret_id.as_bytes()), Field::One(seat.as_bytes())])
}

/// HKDF info of the dealing seed (plan 1.3.3).
#[must_use]
pub fn deal_info(republic_id: &str, name: &str, kind: &str, nonce: &str) -> Vec<u8> {
    layout(
        TAG_DEAL,
        &[
            Field::One(republic_id.as_bytes()),
            Field::One(name.as_bytes()),
            Field::One(kind.as_bytes()),
            Field::One(nonce.as_bytes()),
        ],
    )
}

/// HKDF info that turns the dealing seed into the shared scalar.
#[must_use]
pub fn secret_scalar_info() -> Vec<u8> {
    layout(TAG_SECRET_SCALAR, &[])
}

/// File-plane key info of a payload series (S3a).
#[must_use]
pub fn payload_series_info(payload_hash: &str) -> Vec<u8> {
    layout(TAG_PAYLOAD_SERIES, &[Field::One(payload_hash.as_bytes())])
}

/// File-plane key info of the folded base series (S5).
#[must_use]
pub fn base_series_info(commitment: &str) -> Vec<u8> {
    layout(TAG_BASE_SERIES, &[Field::One(commitment.as_bytes())])
}

/// A grant inside the folded base. The proposal id rides along: without
/// it nobody could recompute `grant_id` after the cut.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultBaseGrant {
    /// The proposal the grant committed under.
    pub proposal_id: u64,
    /// The record.
    pub grant: VaultGrant,
}

/// A current deposit inside the folded base, with the grants on it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultBaseDeposit {
    /// The current version under `(depositor, name)`.
    pub deposit: VaultDeposit,
    /// Grants on exactly this version.
    pub grants: Vec<VaultBaseGrant>,
}

/// The folded vault (spec §9.3): every current deposit and its grants.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct VaultBase {
    /// Current deposits.
    pub deposits: Vec<VaultBaseDeposit>,
}

fn record_json(op: &VaultOp) -> Vec<u8> {
    // through Value: sorted keys, the checkpoint groups' canonical JSON
    let v = serde_json::to_value(op).expect("a vault record serializes");
    serde_json::to_vec(&v).expect("a serde_json::Value serializes")
}

fn put_u64(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(
        &u64::try_from(n)
            .expect("field exceeds the u32/u64 framing - ambiguous signed bytes are never written")
            .to_le_bytes(),
    );
}

/// What a vault cut commits to: deposits in `(depositor, name)` order,
/// each followed by its grants in `grant_id` order; a record is the
/// canonical JSON of its Applied op.
#[must_use]
pub fn vault_base_canonical_bytes(base: &VaultBase) -> Vec<u8> {
    let mut deposits: Vec<&VaultBaseDeposit> = base.deposits.iter().collect();
    deposits.sort_by(|a, b| {
        (&a.deposit.depositor, &a.deposit.name).cmp(&(&b.deposit.depositor, &b.deposit.name))
    });
    let mut out = Vec::new();
    out.extend_from_slice(TAG_BASE.as_bytes());
    out.push(0);
    put_u64(&mut out, deposits.len());
    for d in deposits {
        crate::put_bytes(&mut out, &record_json(&VaultOp::Deposit(d.deposit.clone())));
        let mut grants: Vec<&VaultBaseGrant> = d.grants.iter().collect();
        grants.sort_by(|a, b| a.grant.grant_id.cmp(&b.grant.grant_id));
        put_u64(&mut out, grants.len());
        for g in grants {
            out.extend_from_slice(&g.proposal_id.to_le_bytes());
            crate::put_bytes(&mut out, &record_json(&VaultOp::Grant(g.grant.clone())));
        }
    }
    out
}

/// Read back [`vault_base_canonical_bytes`]: format only, and only the
/// canonical form (sorted, no duplicates, canonical JSON, no trailing
/// bytes), so bytes and base map one to one. The content check is
/// `molt_vault::verify_base`.
///
/// # Errors
/// Any deviation from the canonical stream.
pub fn decode_vault_base(bytes: &[u8]) -> Result<VaultBase, String> {
    let mut tag = TAG_BASE.as_bytes().to_vec();
    tag.push(0);
    let rest = bytes
        .strip_prefix(tag.as_slice())
        .ok_or_else(|| "not a molt-vault-base-v1 stream".to_string())?;
    let mut r = Reader { rest, at: 0 };
    let count = r.u64("the deposit count")?;
    let mut base = VaultBase::default();
    let mut last_slot: Option<(String, String)> = None;
    for _ in 0..count {
        let raw = r.field("a deposit")?;
        let deposit = match decode_record(raw)? {
            VaultOp::Deposit(d) => d,
            _ => return Err("a deposit slot holds another op".to_string()),
        };
        let slot = (deposit.depositor.clone(), deposit.name.clone());
        if last_slot.as_ref().is_some_and(|l| *l >= slot) {
            return Err("deposits out of order".to_string());
        }
        last_slot = Some(slot);
        let n = r.u64("a grant count")?;
        let mut grants = Vec::new();
        let mut last_id: Option<String> = None;
        for _ in 0..n {
            let proposal_id = r.u64("a proposal id")?;
            let raw = r.field("a grant")?;
            let grant = match decode_record(raw)? {
                VaultOp::Grant(g) => g,
                _ => return Err("a grant slot holds another op".to_string()),
            };
            if last_id.as_ref().is_some_and(|l| *l >= grant.grant_id) {
                return Err("grants out of order".to_string());
            }
            last_id = Some(grant.grant_id.clone());
            grants.push(VaultBaseGrant { proposal_id, grant });
        }
        base.deposits.push(VaultBaseDeposit { deposit, grants });
    }
    if r.at != r.rest.len() {
        return Err("trailing bytes after the base".to_string());
    }
    Ok(base)
}

fn decode_record(raw: &[u8]) -> Result<VaultOp, String> {
    let op: VaultOp = serde_json::from_slice(raw).map_err(|_| "a record is not a vault op".to_string())?;
    if record_json(&op) != raw {
        return Err("a record is not canonical".to_string());
    }
    Ok(op)
}

struct Reader<'a> {
    rest: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or_else(|| format!("{what} overruns the stream"))?;
        let slice = self.rest.get(self.at..end).ok_or_else(|| format!("{what} overruns the stream"))?;
        self.at = end;
        Ok(slice)
    }

    fn u64(&mut self, what: &str) -> Result<u64, String> {
        let raw = self.take(8, what)?;
        Ok(u64::from_le_bytes(raw.try_into().map_err(|_| format!("{what} is truncated"))?))
    }

    fn field(&mut self, what: &str) -> Result<&'a [u8], String> {
        let raw = self.take(4, what)?;
        let len = u32::from_le_bytes(raw.try_into().map_err(|_| format!("{what} is truncated"))?);
        self.take(usize::try_from(len).map_err(|_| format!("{what} overruns the stream"))?, what)
    }
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
        fn wiped_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        wiped_on_drop::<SecretHex>();
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
            ..VaultDeposit::default()
        });
        let replace = match &dep {
            VaultOp::Deposit(d) => VaultOp::Deposit(VaultDeposit { replaces: "p".to_string(), ..d.clone() }),
            _ => unreachable!(),
        };
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
            (
                replace,
                json!({
                    "op": "deposit", "depositor": "a", "name": "one", "kind": "text", "replaces": "p", "m": 2,
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
            (VaultRefusal::NeedsNewerRepublic, "needs a newer republic"),
            (VaultRefusal::NeedsNewerVersion("b".to_string()), "needs a newer version: b"),
            (VaultRefusal::NotVerified, "not verified"),
            (VaultRefusal::PayloadNotHeld, "payload not held"),
            (VaultRefusal::NotTheReader, "not the reader"),
            (VaultRefusal::NoVault, "no vault"),
            (VaultRefusal::NoVaultKey, "no vault key"),
            (VaultRefusal::UseVaultSeal, "use vault_seal"),
            (VaultRefusal::TooLarge, "too large"),
            (VaultRefusal::UnknownOp, "unknown op"),
            (VaultRefusal::Stale, "stale version"),
        ];
        for (r, want) in cases {
            assert_eq!(r.to_string(), want);
        }
        assert_eq!(
            crate::MoltError::Vault(VaultRefusal::NoVaultKey).to_string(),
            "vault: no vault key"
        );
    }

    /// The Organization modal's switch rides `status`; an older reader
    /// without the field offers nothing.
    #[test]
    fn vault_enable_wire_shape_is_pinned() {
        for (e, want) in [
            (VaultEnable::On, "\"on\""),
            (VaultEnable::Offer, "\"offer\""),
            (VaultEnable::NeedsNewerRepublic, "\"needs_newer_republic\""),
            (VaultEnable::Bounds, "\"bounds\""),
        ] {
            assert_eq!(serde_json::to_string(&e).expect("json"), want);
        }
        assert_eq!(VaultEnable::default(), VaultEnable::NeedsNewerRepublic);
    }

    // --- layouts -------------------------------------------------------

    /// The layout written out by hand, independent of `layout()`.
    fn by_hand(tag: &str, fields: &[&[&[u8]]]) -> Vec<u8> {
        let mut out = tag.as_bytes().to_vec();
        out.push(0);
        out.extend_from_slice(&u32::try_from(fields.len()).expect("small").to_le_bytes());
        for f in fields {
            // a one-element slice is a plain field; lists are marked by the
            // caller with an explicit count below
            for part in *f {
                out.extend_from_slice(&u32::try_from(part.len()).expect("small").to_le_bytes());
                out.extend_from_slice(part);
            }
        }
        out
    }

    fn run(items: &[&str]) -> Vec<u8> {
        let mut out = u32::try_from(items.len()).expect("small").to_le_bytes().to_vec();
        for i in items {
            out.extend_from_slice(&u32::try_from(i.len()).expect("small").to_le_bytes());
            out.extend_from_slice(i.as_bytes());
        }
        out
    }

    fn digest(bytes: &[u8]) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes))
    }

    fn fixture_deposit() -> VaultDeposit {
        VaultDeposit {
            depositor: "a".to_string(),
            name: "one".to_string(),
            kind: "text".to_string(),
            replaces: "p".to_string(),
            m: 2,
            holders: vec!["b".to_string(), "c".to_string(), "d".to_string()],
            commitments: vec!["c0".to_string(), "c1".to_string()],
            enc_share: vec!["e0".to_string(), "e1".to_string(), "e2".to_string()],
            payload: VaultPayloadRef { hash: "h".to_string(), size: 41 },
            nonce: "n".to_string(),
            sig_depositor: "s".to_string(),
        }
    }

    /// Each layout against its hand-built bytes AND a golden digest: a
    /// layout change without a tag bump goes red here.
    fn pin(got: &[u8], want: &[u8], golden: &str) {
        assert_eq!(got, want);
        assert_eq!(digest(got), golden);
    }

    #[test]
    fn byte_pins_vault_key_info() {
        pin(
            &vault_key_info("npk", "ipk"),
            &by_hand(TAG_KEY, &[&[b"npk"], &[b"ipk"]]),
            "7e4e68451f8e46ca3369b12f04af470d0603c49be4e5fb682e12d84a5980112d",
        );
    }

    #[test]
    fn byte_pins_secret_id() {
        let dep = fixture_deposit();
        let mut want = by_hand(TAG_SECRET, &[&[b"r"], &[b"a"], &[b"one"], &[b"text"], &[b"p"], &[&[2]]]);
        // the count says 9 fields; the two runs and the hash follow
        want.splice(TAG_SECRET.len() + 1..TAG_SECRET.len() + 5, 9u32.to_le_bytes());
        want.extend(run(&["b", "c", "d"]));
        want.extend(run(&["c0", "c1"]));
        want.extend_from_slice(&1u32.to_le_bytes());
        want.extend_from_slice(b"h");
        pin(&secret_id_bytes("r", &dep), &want, "8c52faa1a3b49b36adc4ce9554fdb9f99bab641407aec6aa5ef915fefa3650cf");
        assert_eq!(secret_id("r", &dep), digest(&want));
        // enc_share, nonce, size and the signature stay outside the id
        let mut other = dep.clone();
        other.enc_share = vec!["x".to_string()];
        other.nonce = "y".to_string();
        other.sig_depositor = "z".to_string();
        other.payload.size = 9;
        assert_eq!(secret_id("r", &other), secret_id("r", &dep));
        other.replaces = "q".to_string();
        assert_ne!(secret_id("r", &other), secret_id("r", &dep));
    }

    #[test]
    fn byte_pins_deposit_signing_bytes() {
        let dep = fixture_deposit();
        let mut want = by_hand(TAG_DEPOSIT, &[&[b"r"], &[b"a"], &[b"one"], &[b"text"], &[b"p"], &[&[2]]]);
        want.splice(TAG_DEPOSIT.len() + 1..TAG_DEPOSIT.len() + 5, 12u32.to_le_bytes());
        want.extend(run(&["b", "c", "d"]));
        want.extend(run(&["c0", "c1"]));
        want.extend(run(&["e0", "e1", "e2"]));
        for f in [b"h".as_slice(), &41u64.to_le_bytes(), b"n"] {
            want.extend_from_slice(&u32::try_from(f.len()).expect("small").to_le_bytes());
            want.extend_from_slice(f);
        }
        pin(&deposit_signing_bytes("r", &dep), &want, "61a5a9fb282809aad096ad9a14d551790d497aba4363162d0a6595f183d2984a");
        // the signature is the only field left out
        let mut other = dep.clone();
        other.sig_depositor = "z".to_string();
        assert_eq!(deposit_signing_bytes("r", &other), deposit_signing_bytes("r", &dep));
        for change in [
            |d: &mut VaultDeposit| d.enc_share[0] = "x".to_string(),
            |d: &mut VaultDeposit| d.nonce = "x".to_string(),
            |d: &mut VaultDeposit| d.payload.size = 1,
            |d: &mut VaultDeposit| d.replaces = "q".to_string(),
        ] {
            let mut other = dep.clone();
            change(&mut other);
            assert_ne!(deposit_signing_bytes("r", &other), deposit_signing_bytes("r", &dep));
        }
    }

    #[test]
    fn byte_pins_grant_id() {
        let want = by_hand(TAG_GRANT, &[&[b"s1"], &[b"b"], &[&7u64.to_le_bytes()]]);
        pin(&grant_id_bytes("s1", "b", 7), &want, "c745491c059b4f8a971d25c3364ab43a07adc43c4c5da34dc07da97c44e89425");
        assert_eq!(grant_id("s1", "b", 7), digest(&want));
        assert_ne!(grant_id("s1", "b", 7), grant_id("s1", "b", 8));
    }

    #[test]
    fn byte_pins_share_resp_eph() {
        pin(&share_aad("s1", "b"), &by_hand(TAG_SHARE, &[&[b"s1"], &[b"b"]]), "1437f9a4c86339ad4cd801d4a7bd51361dac5695e1d3ca3243353e8340b3e47b");
        pin(&resp_aad("r", "g1", "b"), &by_hand(TAG_RESP, &[&[b"r"], &[b"g1"], &[b"b"]]), "04280147db4f047d6f2ac55b4470e2de2689bc2abc220ad3271e89367d04ca5a");
        pin(&eph_info("s1", "b"), &by_hand(TAG_EPH, &[&[b"s1"], &[b"b"]]), "a1bd4bf49ee3eaae2d929be809e4fe7c11abf18def59b151b295243127bb278b");
    }

    #[test]
    fn byte_pins_payload_dek_deal() {
        let four: [&[&[u8]]; 4] = [&[b"r"], &[b"a"], &[b"one"], &[b"text"]];
        pin(&payload_aad("r", "a", "one", "text"), &by_hand(TAG_PAYLOAD, &four), "d5bb016aa817cf326b6aa26da13d644f1423c124a977b43482b787fb6557e725");
        pin(&dek_info("r", "a", "one", "text"), &by_hand(TAG_DEK, &four), "f5c914113ec2694de9e4f3861ddf9b66f85646957c45d9594d86ae92b01330e6");
        pin(
            &deal_info("r", "one", "text", "n"),
            &by_hand(TAG_DEAL, &[&[b"r"], &[b"one"], &[b"text"], &[b"n"]]),
            "85f54af9636404aa98b3fce07ef5542fccdcf9b7d45e00b9acb0d852b7e1d08d",
        );
        pin(&secret_scalar_info(), &by_hand(TAG_SECRET_SCALAR, &[]), "9dc73bce8afffe907ff02d220b4eccf6acdd599fd7f0e704c56d5ca5f0d1f131");
    }

    #[test]
    fn byte_pins_series_infos() {
        pin(&payload_series_info("h"), &by_hand(TAG_PAYLOAD_SERIES, &[&[b"h"]]), "8ef2a63f5cf34f76d531a9b663e1768a86d576fca5f56dd50c9d6156874d1bc8");
        pin(&base_series_info("h"), &by_hand(TAG_BASE_SERIES, &[&[b"h"]]), "1ff23e314a0cc5b7b7366b1b9297b6525c8f042584ae9431e7740c798dd0c343");
    }

    fn fixture_base() -> VaultBase {
        let mut second = fixture_deposit();
        second.depositor = "b".to_string();
        let grant = |id: &str, pid| VaultBaseGrant {
            proposal_id: pid,
            grant: VaultGrant { grant_id: id.to_string(), secret_id: "s".to_string(), reader: "c".to_string() },
        };
        // deliberately out of order: the encoder sorts
        VaultBase {
            deposits: vec![
                VaultBaseDeposit { deposit: second, grants: vec![] },
                VaultBaseDeposit { deposit: fixture_deposit(), grants: vec![grant("g2", 9), grant("g1", 4)] },
            ],
        }
    }

    #[test]
    fn byte_pins_vault_base() {
        let base = fixture_base();
        let rec = |op: VaultOp| {
            serde_json::to_vec(&serde_json::to_value(op).expect("value")).expect("bytes")
        };
        let field = |out: &mut Vec<u8>, b: &[u8]| {
            out.extend_from_slice(&u32::try_from(b.len()).expect("small").to_le_bytes());
            out.extend_from_slice(b);
        };
        let mut want = b"molt-vault-base-v1\0".to_vec();
        want.extend_from_slice(&2u64.to_le_bytes());
        field(&mut want, &rec(VaultOp::Deposit(base.deposits[1].deposit.clone())));
        want.extend_from_slice(&2u64.to_le_bytes());
        for g in [&base.deposits[1].grants[1], &base.deposits[1].grants[0]] {
            want.extend_from_slice(&g.proposal_id.to_le_bytes());
            field(&mut want, &rec(VaultOp::Grant(g.grant.clone())));
        }
        field(&mut want, &rec(VaultOp::Deposit(base.deposits[0].deposit.clone())));
        want.extend_from_slice(&0u64.to_le_bytes());
        // sorted keys: the record is the checkpoint groups' canonical JSON
        assert!(String::from_utf8_lossy(&want).contains(r#"{"commitments":["c0","c1"],"depositor":"a""#));
        pin(&vault_base_canonical_bytes(&base), &want, "69664a934554df7a8a73f3511908dafe41e8c31278d7250432f47f97d13b4f33");
        let empty = vault_base_canonical_bytes(&VaultBase::default());
        let mut want_empty = b"molt-vault-base-v1\0".to_vec();
        want_empty.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(empty, want_empty);
    }

    #[test]
    fn the_vault_base_decodes_only_its_canonical_form() {
        let base = fixture_base();
        let bytes = vault_base_canonical_bytes(&base);
        let back = decode_vault_base(&bytes).expect("decodes");
        assert_eq!(vault_base_canonical_bytes(&back), bytes);
        assert_eq!(back.deposits[0].deposit.depositor, "a");
        assert_eq!(back.deposits[0].grants[0].grant.grant_id, "g1");
        assert_eq!(decode_vault_base(&vault_base_canonical_bytes(&VaultBase::default())), Ok(VaultBase::default()));

        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_vault_base(&trailing).is_err());
        assert!(decode_vault_base(&bytes[..bytes.len() - 1]).is_err());
        assert!(decode_vault_base(b"molt-vault-base-v2\0").is_err());

        // a duplicate slot, a duplicate grant: not canonical
        let mut dup = VaultBase { deposits: vec![base.deposits[1].clone(), base.deposits[1].clone()] };
        assert!(decode_vault_base(&vault_base_canonical_bytes(&dup)).is_err());
        dup.deposits.truncate(1);
        let g = dup.deposits[0].grants[0].clone();
        dup.deposits[0].grants.push(g);
        assert!(decode_vault_base(&vault_base_canonical_bytes(&dup)).is_err());

        // non-canonical JSON (spaces) and a grant in a deposit slot
        let mut loose = b"molt-vault-base-v1\0".to_vec();
        loose.extend_from_slice(&1u64.to_le_bytes());
        let raw = br#"{"op": "grant", "grant_id": "g", "secret_id": "s", "reader": "b"}"#;
        loose.extend_from_slice(&u32::try_from(raw.len()).expect("small").to_le_bytes());
        loose.extend_from_slice(raw);
        loose.extend_from_slice(&0u64.to_le_bytes());
        assert!(decode_vault_base(&loose).is_err());
    }

    #[test]
    fn layouts_are_injective_across_field_boundaries() {
        let two: [fn(&str, &str) -> Vec<u8>; 5] = [
            vault_key_info,
            share_aad,
            eph_info,
            |a, b| resp_aad("r", a, b),
            |a, b| grant_id_bytes(a, b, 1),
        ];
        for f in two {
            assert_ne!(f("ab", "c"), f("a", "bc"));
        }
        type Four = fn(&str, &str, &str, &str) -> Vec<u8>;
        let four: [Four; 3] = [payload_aad, dek_info, deal_info];
        for f in four {
            assert_ne!(f("ab", "c", "d", "e"), f("a", "bc", "d", "e"));
            assert_ne!(f("a", "bc", "d", "e"), f("a", "b", "cd", "e"));
            assert_ne!(f("a", "b", "cd", "e"), f("a", "b", "c", "de"));
        }
        // a holder moved across the run boundary
        let dep = fixture_deposit();
        let mut moved = dep.clone();
        moved.holders = vec!["b".to_string(), "c".to_string()];
        moved.commitments = vec!["d".to_string(), "c0".to_string(), "c1".to_string()];
        assert_ne!(secret_id_bytes("r", &moved), secret_id_bytes("r", &dep));
        assert_ne!(deposit_signing_bytes("r", &moved), deposit_signing_bytes("r", &dep));
    }

    #[test]
    fn no_two_primitives_share_a_tag() {
        let mut seen = std::collections::BTreeSet::new();
        for t in VAULT_TAGS {
            assert!(seen.insert(t), "{t} twice");
            assert!(t.starts_with("molt-vault-") && t.ends_with("-v1"), "{t}");
        }
        // and every layout leads with its own tag
        let dep = fixture_deposit();
        let outputs = [
            (TAG_KEY, vault_key_info("a", "b")),
            (TAG_SECRET, secret_id_bytes("r", &dep)),
            (TAG_DEPOSIT, deposit_signing_bytes("r", &dep)),
            (TAG_GRANT, grant_id_bytes("s", "b", 1)),
            (TAG_SHARE, share_aad("s", "b")),
            (TAG_RESP, resp_aad("r", "g", "b")),
            (TAG_PAYLOAD, payload_aad("r", "a", "n", "k")),
            (TAG_DEK, dek_info("r", "a", "n", "k")),
            (TAG_EPH, eph_info("s", "b")),
            (TAG_DEAL, deal_info("r", "n", "k", "x")),
            (TAG_SECRET_SCALAR, secret_scalar_info()),
            (TAG_BASE, vault_base_canonical_bytes(&VaultBase::default())),
            (TAG_PAYLOAD_SERIES, payload_series_info("h")),
            (TAG_BASE_SERIES, base_series_info("h")),
        ];
        assert_eq!(outputs.len(), VAULT_TAGS.len());
        for (tag, bytes) in outputs {
            let mut head = tag.as_bytes().to_vec();
            head.push(0);
            assert!(bytes.starts_with(&head), "{tag}");
        }
    }
}
