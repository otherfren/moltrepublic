// SPDX-License-Identifier: GPL-3.0-or-later

//! The gated surfaces: propose → threshold approvals → apply.
//!
//! Two honest paths, no simulation: a **chain-governed** republic (every
//! ritual-founded workspace) runs real signed m-of-n threshold governance
//! over the mesh (`chain.rs`); every other context (the solo boot group,
//! legacy pre-chain workspaces, session-only tests) runs the
//! **single-operator** path — this node records at most its OWN approval,
//! and a proposal applies only when that one real vote meets the
//! threshold (the honest 1-of-1 case). Counting repeated local approvals
//! as invented peers — the pre-chain simulation — is gone: a repeat is
//! refused with [`MoltError::AlreadyApproved`].

use std::collections::HashMap;

use molt_core::{
    ChannelInfo, ChannelRef, Event, MemberView, MemberVote, MoltError, ProposalId,
    ProposalRecord, ProposalState, ProposalView, Reply, StatusView, Surface, SurfaceSnapshot,
    SurfaceStat, UploadView, VoteState, WorkspaceEvent,
};
use serde_json::Value;

use crate::State;

/// Parked declines: at most this many proposal ids wait for their proposal
/// (each id holds at most one voice per member) — a roster member can spam
/// declines for invented ids, and the park must not grow with them.
const PARKED_DECLINE_IDS_MAX: usize = 1024;

/// Per-member ceiling on parked decline ids: a single member wedging the
/// park with invented ids must not evict or block the other members'
/// honest out-of-order voices (the frames are acked at the accept point,
/// so a shed voice would be gone for good — review 2026-08-09).
pub(crate) const PARKED_DECLINES_PER_MEMBER_MAX: usize = 64;

/// How far beyond the highest known proposal id a decline may point and
/// still park: a real decline references an id some proposer minted and
/// gossiped, so anything far past `next_id` is garbage — and bounding it
/// keeps a hostile id (u64::MAX) from poisoning the mint counter.
pub(crate) const PARKED_DECLINE_ID_WINDOW: u64 = 1024;

/// What registering a decline did. The wire ingest emits from this; the log
/// applier ignores it (replay must not ring frontends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeclineOutcome {
    /// A fresh voice against; the proposal stays open.
    Voice,
    /// A fresh voice, and it tipped the proposal terminal.
    Rejected,
    /// Nothing new (duplicate voice, or the proposal is already terminal).
    Known,
    /// The proposal is unknown here — the voice parked for its arrival.
    Parked,
}

/// What a park drain registered (D4): each drained voice by name, so the
/// LIVE wire arm can emit one `Event::Declined` per member (the applier
/// callers discard — replay must not ring frontends), plus whether the
/// drain tipped the card terminal.
pub(crate) struct ParkDrain {
    /// The members whose parked voices registered, in park order.
    pub(crate) voices: Vec<molt_core::MemberId>,
    /// The drain turned the card Rejected.
    pub(crate) rejected: bool,
}

/// What one withdraw registration did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WithdrawOutcome {
    /// The card turned terminal (withdrawn).
    Withdrawn,
    /// The proposal is unknown here — the verdict parked.
    Parked,
    /// Duplicate, terminal already, or not the proposer — nothing changed.
    Ignored,
}

/// The republic's EFFECTIVE display state: the ratified genesis folded
/// with the applied Organization ops (last write wins per op). This is
/// what every reader shows — the genesis itself stays immutable history,
/// it is only the fold's floor. Display/read state, never consensus input.
pub(crate) struct OrgEffective {
    /// Last applied `set_name`, founding name until one applies.
    pub name: String,
    /// Last applied `set_charter`, founding agenda until one applies.
    pub agenda: String,
    /// Last applied `set_chat_retention` in days (default 7): the "delete
    /// chat after" window the read contract filters on.
    pub retention_days: u64,
    /// Last applied `set_image` reference, cleared by `remove_image`.
    pub image: String,
    /// The effective governed pool (R6), space-joined for display — the
    /// ratified founding pool until a `set_relays` edit applies.
    pub relays: String,
    /// The effective feature set (`charter_features.md`), space-joined for
    /// display — the ratified founding selection unioned with every applied
    /// `set_features` edit (enable-only by fold).
    pub features: String,
}

/// Parse a retention window ("14 days" or a bare "14") into days.
/// `None` when unparseable or outside 1..=365 — callers refuse, never guess.
pub(crate) fn parse_retention_days(value: &str) -> Option<u64> {
    let days: u64 = value.split_whitespace().next()?.parse().ok()?;
    (1..=365).contains(&days).then_some(days)
}

/// **What a proposal may cost on the wire — derived, never chosen.**
///
/// An applied proposal travels as a `Committed` block inside ONE kind-445
/// frame, and `RelayRuntime::publish` refuses an over-budget frame *locally
/// and deterministically*, before any relay is contacted. The outbox then
/// holds its cursor at that envelope on purpose — nothing recovers a skipped
/// one — so a payload that cannot be framed inside the budget wedges
/// everything the node writes after it, across restarts.
///
/// The gate therefore belongs HERE, at propose time, where a human can still
/// pick a smaller image or a shorter charter.
///
/// `DEFAULT_SIZE_BUDGET` rather than a probed pool value on purpose: every
/// member must reach the SAME verdict on the same proposal, and a per-node
/// probe would make one node's proposal another node's silent refusal. When
/// pool probing lands (`with_size_budget` has no production caller yet), a
/// SMALLER probed budget belongs here as a second, node-local refusal — not
/// as a replacement for this shared one.
fn transport_plaintext_ceiling() -> usize {
    molt_net::envelope::max_plaintext_for(molt_net::relay_runtime::DEFAULT_SIZE_BUDGET)
}

/// The plaintext an applied `payload` costs, in the WORST case this republic
/// can produce: the highest sequence numbers, and an attestation from every
/// seat. `n` and not the threshold `m` because a block may carry more than
/// `m` signatures, and the seat count is fixed at founding — so this is the
/// honest bound, not a pessimistic one.
pub(crate) fn applied_block_plaintext_len(
    surface: Surface,
    payload: &Value,
    roster: &[molt_core::MemberId],
) -> usize {
    let block = molt_core::ChainBlock {
        height: u64::MAX,
        prev: "f".repeat(64),
        change: molt_core::chain::ChainChange::Applied {
            proposal_id: u64::MAX,
            surface,
            payload: payload.clone(),
        },
        sigs: roster
            .iter()
            .map(|m| molt_core::RosterAttestation {
                member: m.clone(),
                sig: "f".repeat(128),
            })
            .collect(),
    };
    let by = roster.iter().max_by_key(|m| m.len()).cloned().unwrap_or_default();
    let env = molt_core::EventEnvelope {
        seq: u64::MAX,
        prev_seq: u64::MAX,
        ts: u64::MAX,
        by,
        body: WorkspaceEvent::Committed(block),
    };
    // a payload that will not even serialize cannot be published either
    serde_json::to_vec(&env).map_or(usize::MAX, |v| v.len())
}

/// Can this payload ever become a publishable block? The shared verdict —
/// the propose path refuses on it, and the wire guard drops on it.
pub(crate) fn payload_fits(
    surface: Surface,
    payload: &Value,
    roster: &[molt_core::MemberId],
) -> bool {
    applied_block_plaintext_len(surface, payload, roster) <= transport_plaintext_ceiling()
}

/// How many DECODED image bytes still fit beside the rest of `payload` —
/// what the refusal names, so "too large" arrives with the number to aim at.
fn image_headroom(
    surface: Surface,
    payload: &Value,
    roster: &[molt_core::MemberId],
) -> usize {
    let mut bare = payload.clone();
    if let Some(obj) = bare.as_object_mut() {
        obj.insert("bytes_b64".into(), Value::String(String::new()));
    }
    let base = applied_block_plaintext_len(surface, &bare, roster);
    // what is left is base64 capacity: 4 encoded bytes carry 3 decoded ones
    transport_plaintext_ceiling().saturating_sub(base) / 4 * 3
}

/// Refuse a proposal the transport could never carry. Names the ONE thing
/// the human can change — for an image proposal that is the image, for
/// anything else the overshoot.
fn validate_payload_fits(
    surface: Surface,
    payload: &Value,
    roster: &[molt_core::MemberId],
) -> Result<(), MoltError> {
    if payload_fits(surface, payload, roster) {
        return Ok(());
    }
    // the image wording belongs to a set_image and nothing else: any payload
    // may happen to carry a `bytes_b64`, and telling its author to shrink an
    // image they never proposed is a message that costs them the fix
    let is_set_image = surface == Surface::Organization
        && matches!(
            payload.get("op").and_then(Value::as_str),
            Some("set_image" | "set_member_image")
        );
    if let Some(bytes) = image_bytes(payload).filter(|_| is_set_image) {
        return Err(MoltError::BadPayload(format!(
            "the image is {} KiB - this republic's relays carry {} KiB",
            bytes.len() / 1024,
            image_headroom(surface, payload, roster) / 1024
        )));
    }
    let over = applied_block_plaintext_len(surface, payload, roster)
        .saturating_sub(transport_plaintext_ceiling());
    Err(MoltError::BadPayload(format!(
        "the proposal is {} KiB over what this republic's relays carry",
        over.div_ceil(1024)
    )))
}

/// The logo file extension for a proposed image: taken from the display
/// value's extension (lowercased, alphanumeric, short), "png" otherwise —
/// the fold and the writer must derive the SAME name.
pub(crate) fn logo_ext(value: &str) -> String {
    let ext = value.rsplit('.').next().unwrap_or_default().to_lowercase();
    if !ext.is_empty()
        && ext.len() <= 5
        && ext != value
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
    {
        ext
    } else {
        "png".to_string()
    }
}

/// The file-name room the served image budget reserves. A picture proposal
/// also carries its display name, and the budget is quoted BEFORE the name
/// is known — so the probe pays for a long one and the promise stays keepable
/// for any name up to this length. The propose gate remains the authority.
const AVATAR_NAME_ALLOWANCE: usize = 64;

/// Decoded picture bytes a `set_member_image` still fits in a republic of
/// this roster — what [`molt_core::StatusView::image_budget`] serves the
/// frontends as their downscale target (`member_profiles_plan.md` (a)).
/// Conservative by [`AVATAR_NAME_ALLOWANCE`]: what it promises is accepted.
pub(crate) fn member_image_budget(member: &str, roster: &[molt_core::MemberId]) -> usize {
    let probe = serde_json::json!({
        "op": "set_member_image",
        "member": member,
        "value": "x".repeat(AVATAR_NAME_ALLOWANCE),
    });
    image_headroom(Surface::Organization, &probe, roster)
}

/// The member description's character cap (`member_profiles_plan.md` (d)):
/// a table column, not a charter. The payload gate alone would allow tens
/// of thousands of characters.
pub(crate) const DESC_MAX: usize = 500;

/// The dimension ceiling of the decodability sniff — the same 8192² the
/// molt-ui preview decoder enforces, so the engine never accepts an image
/// the GUI could not render.
const IMAGE_MAX_DIM: u32 = 8192;

/// WP3: can these bytes become a picture on every member's screen? A cheap
/// sniff, never a full decode (decode bombs): `guess_format` reads magic
/// bytes, `into_dimensions` reads only the header, and the dimensions are
/// capped. An SVG (not an `image`-crate format) travels as source text and
/// is sniffed by prefix. Sign-what-you-see runs empty if a voter can only
/// ever get the "cannot decode" toast — so refuse at propose, drop on the
/// wire. (The GUI additionally runs its real preview decoder before
/// proposing — deliberate duplication: the UI really renders, this engine
/// gate is the co-equal contract every frontend and the wire fold share.)
pub(crate) fn image_decodable(bytes: &[u8]) -> Result<(), MoltError> {
    image_dimensions(bytes).map(|_| ())
}

/// [`image_decodable`]'s sniff, with the header dimensions it already read
/// handed back — so a caller that also judges the SHAPE (the square rule
/// for a member picture) reads them once, not twice.
pub(crate) fn image_dimensions(bytes: &[u8]) -> Result<(u32, u32), MoltError> {
    let refuse = || {
        MoltError::BadPayload(
            "the image cannot be decoded (png/jpeg/webp/gif/bmp)".into(),
        )
    };
    // L1 (decided 2026-08-16): SVG is REFUSED, not structurally vetted —
    // the prefix sniff accepted any <svg/<?xml text with no dimension or
    // nesting check, straight into every member's renderer (a 256 KiB
    // billion-laughs body passed). Already-applied SVG logos keep
    // rendering (the read path never re-validates); this gate is
    // propose/wire-only.
    let head = std::str::from_utf8(&bytes[..bytes.len().min(1024)]).unwrap_or("");
    let trimmed = head.trim_start();
    if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
        return Err(MoltError::BadPayload(
            "svg is not accepted - use a raster image (png/jpeg/webp/gif/bmp)".into(),
        ));
    }
    image::guess_format(bytes).map_err(|_| refuse())?;
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| refuse())?
        .into_dimensions()
        .map_err(|_| refuse())?;
    if w == 0 || h == 0 || w > IMAGE_MAX_DIM || h > IMAGE_MAX_DIM {
        return Err(MoltError::BadPayload(format!(
            "the image is {w}x{h} - the limit is {IMAGE_MAX_DIM}x{IMAGE_MAX_DIM}"
        )));
    }
    Ok((w, h))
}

/// One member's EFFECTIVE profile, folded from the applied log. The
/// description is BORROWED out of the log entry; the image is the reference
/// the frontends render (a materialized file path, or the display value on
/// a session-only open) — never the bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct MemberProfile<'a> {
    /// The applied picture's local file path ("" = none).
    pub image: String,
    /// The applied description ("" = none).
    pub desc: &'a str,
}

/// The per-member avatar file name: readable slug plus a stable hash of the
/// exact member name, so two seats whose slugs collide ("Anna B" / "anna-b")
/// still get their own file. A file name, never key material.
///
/// The slug is trimmed to the ASCII that `molt_storage`'s file-name guard
/// accepts. `slugify` keeps every UNICODE alphanumeric and lowercases it,
/// and some lowercase expansions add a combining mark - Turkish dotted I
/// becomes "i\u{307}" - which the guard refuses, so that seat's picture
/// would fail to materialize on every node, forever. A name with no ASCII at
/// all (CJK) keeps just the hash: the slug is readability, the hash is
/// identity.
pub(crate) fn avatar_stem(member: &str) -> String {
    let slug: String = molt_core::slugify(member)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    // sha256, not fnv1a64: the suffix is what keeps two seats apart, and
    // fnv is step-invertible - a member choosing their own handle at join
    // could meet-in-the-middle onto another seat's suffix and, with a
    // colliding slug, capture or destroy that member's picture file.
    let hash = &molt_storage::content_hash(member.as_bytes())[..24];
    match slug.trim_matches('-') {
        "" => hash.to_string(),
        s => format!("{s}-{hash}"),
    }
}

/// `avatar-<stem>.<ext>` — the fold and the writer must derive the SAME
/// name, so both go through here (the org twin is `logo.<ext>`).
pub(crate) fn avatar_file_name(member: &str, value: &str) -> String {
    format!("avatar-{}.{}", avatar_stem(member), logo_ext(value))
}

/// Is this an Organization payload editing ONE member's profile? The three
/// ops that carry a `member` and may only ever be proposed by that member
/// (`member_profiles_plan.md` §2) — the propose gate and the wire drop share
/// this one answer.
pub(crate) fn is_member_profile_op(payload: &Value) -> bool {
    matches!(
        payload.get("op").and_then(Value::as_str),
        Some("set_member_image" | "remove_member_image" | "set_member_desc")
    )
}

/// A member picture must also be SQUARE (`member_profiles_plan.md` (h)):
/// every frontend renders it in a square box, so a non-square one would be
/// cropped differently per client — the contract belongs in the engine, not
/// in one frontend. The org logo keeps taking any shape.
/// A description is a table column, not a charter. ONE rule for both doors:
/// the propose gate and the wire drop call this, so a peer can never post a
/// description the local gate would refuse (`member_profiles_plan.md` §2).
pub(crate) fn member_desc_ok(value: &str) -> Result<(), MoltError> {
    let len = value.trim().chars().count();
    if len > DESC_MAX {
        return Err(MoltError::BadPayload(format!(
            "the description is {len} characters - the limit is {DESC_MAX}"
        )));
    }
    Ok(())
}

pub(crate) fn member_image_ok(bytes: &[u8]) -> Result<(), MoltError> {
    let (w, h) = image_dimensions(bytes)?;
    if w != h {
        return Err(MoltError::BadPayload(format!(
            "the image is {w}x{h} - it must be square"
        )));
    }
    Ok(())
}

/// Decode a `set_image` payload's embedded bytes (`None` when absent,
/// empty or undecodable — the defensive twin of the propose validation).
pub(crate) fn image_bytes(payload: &Value) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let b64 = payload.get("bytes_b64").and_then(Value::as_str)?;
    if b64.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(b64).ok()
}

/// The "Ist-Stand / Soll-Stand" display pair of a proposal: what the
/// targeted state is now (the EFFECTIVE org state, for the Organization
/// edit ops) and what the change would make it (the payload's `value`).
/// Display data, never consensus input — "" when unknown.
pub(crate) fn change_summary(eff: &OrgEffective, p: &ProposalRecord) -> (String, String) {
    let proposed = p
        .payload
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if p.surface != Surface::Organization {
        return (String::new(), proposed);
    }
    let op = p.payload.get("op").and_then(Value::as_str).unwrap_or("");
    let current = match op {
        "set_charter" => eff.agenda.clone(),
        "set_name" => eff.name.clone(),
        // L10: a MACHINE value — the unit is rendered per-language by the
        // frontends (the op-placeholder title rule applied to the value)
        "set_chat_retention" => eff.retention_days.to_string(),
        // the image ops show what they change: the current image reference
        "set_image" | "remove_image" => eff.image.clone(),
        "set_relays" => eff.relays.clone(),
        "set_features" => eff.features.clone(),
        // an op this build doesn't know (ops are free-form wire strings, so
        // an older log may carry one) — tolerated, nothing to show
        _ => String::new(),
    };
    (current, proposed)
}

/// Refuse an Organization edit whose value could never become honest
/// effective state: an applied entry is forever (the log is append-only),
/// so a blank name or an unparseable retention window must not get in.
/// Local proposals only — the wire fold stays defensive on its own.
fn validate_org_payload(surface: Surface, payload: &Value) -> Result<(), MoltError> {
    if surface != Surface::Organization {
        return Ok(());
    }
    let op = payload.get("op").and_then(Value::as_str).unwrap_or("");
    let value = payload.get("value").and_then(Value::as_str).unwrap_or("");
    match op {
        "set_name" if value.trim().is_empty() => Err(MoltError::BadPayload(
            "the republic needs a non-empty name".into(),
        )),
        "set_chat_retention" if parse_retention_days(value).is_none() => {
            Err(MoltError::BadPayload(
                "the retention window must be 1..=365 days (e.g. \"14 days\")".into(),
            ))
        }
        // an image proposal must carry the actual bytes (sign-what-you-see:
        // members vote on the image, not on a path only the proposer has).
        // Its SIZE is judged by `validate_payload_fits`, against the
        // transport budget — there is no second, independently chosen cap.
        "set_image" => match image_bytes(payload) {
            None => Err(MoltError::BadPayload(
                "a set_image proposal must embed the image (base64 `bytes_b64`)".into(),
            )),
            // WP3: the bytes must also decode as a picture
            Some(bytes) => image_decodable(&bytes),
        },
        // the same rule for a member picture, plus the square shape. The
        // seat this names is checked in `cmd_propose` (it needs the roster).
        "set_member_image" => match image_bytes(payload) {
            None => Err(MoltError::BadPayload(
                "a set_member_image proposal must embed the image (base64 `bytes_b64`)".into(),
            )),
            Some(bytes) => member_image_ok(&bytes),
        },
        // a table column, not a charter — the refusal names the limit; an
        // empty value clears the description
        "set_member_desc" => member_desc_ok(value),
        // a feature edit — space-separated optional-surface keys, every one
        // known (an applied entry is forever; the enable-only gate sits in
        // cmd_propose, the union fold makes it deterministic)
        "set_features" => {
            let tokens: Vec<String> =
                value.split_whitespace().map(str::to_string).collect();
            if tokens.is_empty() {
                return Err(MoltError::BadPayload("nothing to enable".into()));
            }
            molt_core::canonical_features(&tokens)
                .map(|_| ())
                .map_err(MoltError::BadPayload)
        }
        // R6: a pool edit — space-separated relay URLs, each one canonical
        // (an applied entry is forever)
        "set_relays" => {
            let tokens: Vec<&str> = value.split_whitespace().collect();
            if tokens.is_empty() {
                return Err(MoltError::BadPayload("the pool needs at least one relay".into()));
            }
            for t in tokens {
                molt_core::relay::normalize_relay_url(t)
                    .map_err(|e| MoltError::BadPayload(format!("{t}: {e}")))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

impl State {
    pub(crate) fn cmd_propose(
        &mut self,
        surface: Surface,
        mut payload: Value,
    ) -> Result<Reply, MoltError> {
        if !surface.is_gated() {
            return Err(MoltError::ChatNotGated);
        }
        // D7: no governance on a surface the charter has not enabled
        // (Organization itself always passes — set_features rides it)
        self.require_feature(surface)?;
        // set_relays: store the CANONICAL spelling (review 2026-08-09). The
        // parser accepts-and-rewrites ":443", a trailing "/" and uppercase;
        // recording the raw token instead would poison every later
        // exact-string compare (the overlap gate refusing a legal
        // migration, relay_splits phantom splits, the diff card marking one
        // relay as remove+add). A token that does not parse stays as typed —
        // validate_org_payload names it in the refusal.
        if surface == Surface::Organization
            && payload.get("op").and_then(Value::as_str) == Some("set_relays")
        {
            let raw = payload.get("value").and_then(Value::as_str).unwrap_or_default();
            let canon: Result<Vec<String>, ()> = raw
                .split_whitespace()
                .map(|t| molt_core::relay::normalize_relay_url(t).map_err(|_| ()))
                .collect();
            if let Ok(tokens) = canon {
                payload["value"] = Value::String(tokens.join(" "));
            }
        }
        // set_features: store the CANONICAL set (sorted + deduped) — one
        // set, one spelling, so the diff card and the union fold never meet
        // two encodings of the same selection
        if surface == Surface::Organization
            && payload.get("op").and_then(Value::as_str) == Some("set_features")
        {
            let raw = payload.get("value").and_then(Value::as_str).unwrap_or_default();
            let canon: std::collections::BTreeSet<&str> = raw.split_whitespace().collect();
            payload["value"] =
                Value::String(canon.into_iter().collect::<Vec<_>>().join(" "));
        }
        // set_member_desc: store the TRIMMED value — one spelling per
        // description, so whitespace-only input clears the field instead of
        // rendering blanks (same canonicalize-at-propose rule as above)
        if surface == Surface::Organization
            && payload.get("op").and_then(Value::as_str) == Some("set_member_desc")
        {
            let raw = payload.get("value").and_then(Value::as_str).unwrap_or_default();
            payload["value"] = Value::String(raw.trim().to_string());
        }
        if !payload.is_object() {
            return Err(MoltError::BadPayload(
                "payload must be a JSON object".into(),
            ));
        }
        // reserved for the engine's membership machinery (recovery approval
        // design, 2026-08-08): a user proposal wearing one of these ops would
        // impersonate a membership record — `proposal_change`,
        // `id_free_for` and `settle_membership_records` key on them
        if matches!(
            payload.get("op").and_then(Value::as_str),
            Some("restore_member" | "add_member")
        ) {
            return Err(MoltError::BadPayload("reserved op".into()));
        }
        // a member profile belongs to its member (`member_profiles_plan.md`
        // §2): the seat must exist, and only its own node may propose the
        // edit. The roster read is why this sits here and not in the
        // stateless payload validation; the wire twin is the drop in
        // `cmd_net_delivered`, where the link identity is the proof.
        if surface == Surface::Organization && is_member_profile_op(&payload) {
            let member = payload.get("member").and_then(Value::as_str).unwrap_or_default();
            if member.is_empty() {
                return Err(MoltError::BadPayload("the profile op needs a member".into()));
            }
            if !self.roster().iter().any(|m| m == member) {
                return Err(MoltError::BadPayload(format!("{member}: not in the roster")));
            }
            if member != self.member() {
                return Err(MoltError::BadPayload(format!(
                    "only {member} can edit this profile"
                )));
            }
        }
        validate_org_payload(surface, &payload)?;
        validate_payload_fits(surface, &payload, &self.roster())?;
        // R6: a pool edit that would strand a DECLARED member — sharing no
        // relay with what that seat is on record as reaching (R3b) — is the
        // R4 split as a proposal. Refuse it naming the member and its relay.
        // Undeclared members follow the governed pool and never gate.
        if surface == Surface::Organization
            && payload.get("op").and_then(Value::as_str) == Some("set_relays")
        {
            let new_pool: Vec<&str> = payload
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            for (m, reach) in &self.chain.member_relays {
                if reach.iter().any(|r| new_pool.contains(&r.as_str())) {
                    continue;
                }
                let named = reach.first().cloned().unwrap_or_default();
                return Err(MoltError::BadPayload(format!(
                    "{m} reaches only {named} - keep it or bridge it"
                )));
            }
            // make-before-break (found live 2026-08-09, AFTER the more
            // specific per-member check): the committing block travels over
            // the OLD pool, and members that applied it rebuild onto the
            // new pool only — zero overlap tears the republic at exactly
            // that commit (a member yet to apply keeps listening where
            // nobody publishes anymore). A full migration is two votes:
            // add the new relay, then drop the old.
            let current = self.effective_relays();
            if !current.is_empty() && !current.iter().any(|r| new_pool.contains(&r.as_str())) {
                return Err(MoltError::BadPayload(format!(
                    "no shared relay with the current pool - keep one (e.g. {}) and drop it in a second vote",
                    current[0]
                )));
            }
        }
        // enable-only (`charter_features.md` D5): the proposed set must keep
        // every effective feature this build KNOWS and add at least one new
        // key. The gate is local courtesy — the union fold is what makes the
        // rule deterministic. Unknown effective keys (a newer build enabled
        // them) are deliberately exempt from the keep-check: this build can
        // neither name them (validate refuses) nor lose them (the fold is a
        // union), and demanding them would brick feature governance on every
        // older build (review 2026-08-12).
        if surface == Surface::Organization
            && payload.get("op").and_then(Value::as_str) == Some("set_features")
        {
            let proposed: std::collections::BTreeSet<&str> = payload
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split_whitespace()
                .collect();
            let current = self.effective_features();
            for f in &current {
                let known = Surface::parse(f).is_some_and(Surface::is_charter_feature);
                if known && !proposed.contains(f.as_str()) {
                    return Err(MoltError::BadPayload(format!("{f}: cannot be disabled")));
                }
            }
            if proposed.iter().all(|k| current.iter().any(|c| c == k)) {
                return Err(MoltError::BadPayload("already enabled".into()));
            }
        }
        let me = self.member();
        let id = ProposalId(self.next_id);
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Proposed {
                id,
                surface,
                payload,
            },
        );
        self.record(env);
        self.emit(Event::Proposed { id, surface, by: me.clone() });
        if self.is_chain_governed() {
            // real threshold: the proposer co-signs their own proposal; the
            // other members' signatures arrive over the mesh
            if self.config.self_cosign {
                self.chain_sign_and_gossip_approval(id.0);
            }
        } else {
            if self.config.self_cosign {
                // single-operator path: the proposer's own co-signature is
                // this node's one real approval — recorded as an event, so
                // replay never depends on the config flag
                let env = self.make_env(
                    me.clone(),
                    WorkspaceEvent::Approved {
                        id,
                        by: me,
                        height: 0,
                        sig: String::new(),
                    },
                );
                self.record(env);
            }
            // the one real vote may already satisfy a threshold of 1 —
            // honest 1-of-1 governance (the solo boot group)
            self.try_apply(id);
        }
        Ok(Reply::Proposed { id })
    }

    /// The single-operator invariant, stated ONCE: without chain governance
    /// the only approval this node can record is the local operator's own —
    /// [`State::cmd_approve`] refuses repeats, and the wire delivery drops
    /// governance frames for non-chain workspaces — and the pre-chain
    /// simulation's legacy logs share the shape (their every `Approved`
    /// frame was minted locally, the first always the operator's). So "any
    /// approval recorded" means "the operator already voted"; the approve
    /// guard, `approved_by_me`, the votes row and [`State::waits_on`] must
    /// all read it from here so they can never drift apart.
    fn operator_approved(p: &ProposalRecord) -> bool {
        p.approvals > 0
    }

    pub(crate) fn cmd_approve(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        let operator_already_voted = {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
            // D7's approve half (review 2026-08-12): the propose gate is
            // local, a peer's proposal on a disabled surface still lands in
            // the pool — but no signature leaves this node for a surface the
            // charter has not enabled, so it can never reach m honest seats.
            // (The nav hides such a surface, so a GUI member could not even
            // SEE the card it would be co-signing.)
            self.require_feature(p.surface)?;
            // D2 (last vote counts, decided 2026-08-16): an approve over
            // the own standing decline RETRACTS the decline below — the
            // newest stance wins, mirroring the decline's signature
            // retraction in `register_decline`.
            Self::operator_approved(p)
        };
        {
            let me = self.member();
            if let Some(p) = self.proposals.get_mut(&proposal.0) {
                p.decliners.retain(|d| d != &me);
            }
        }
        if self.is_chain_governed() {
            // real threshold: sign + gossip; a block seals once m distinct
            // members have signed (here or over the mesh)
            self.chain_sign_and_gossip_approval(proposal.0);
            let have = self.chain_approval_count(proposal.0);
            self.emit(Event::Approved {
                id: proposal,
                have,
                need: self.threshold(),
            });
        } else {
            // single-operator path: this node contributes exactly ONE real
            // approval — its own. A repeat must not count as the next
            // member's co-signature (that was the pre-chain simulation);
            // the missing votes can only come from the members themselves,
            // which takes a chain-governed republic.
            if operator_already_voted {
                return Err(MoltError::AlreadyApproved(proposal));
            }
            let me = self.member();
            let env = self.make_env(
                me.clone(),
                WorkspaceEvent::Approved {
                    id: proposal,
                    by: me,
                    height: 0,
                    sig: String::new(),
                },
            );
            self.record(env);
            let have = self.proposals.get(&proposal.0).map(|p| p.approvals).unwrap_or(0);
            self.emit(Event::Approved {
                id: proposal,
                have,
                need: self.threshold(),
            });
            self.try_apply(proposal);
        }
        Ok(Reply::Ack)
    }

    // WITHDRAW ("pull back", not built yet — the ProposalCard shows the
    // disabled button on own proposals). The later implementation, so it
    // converges like decline does:
    // - `Command::Withdraw { proposal }`, a HUMAN decision → an MCP tool
    //   AND a GUI button (co-equality test will demand one of the two
    //   lists; this is a tool, not INTERNAL).
    // - Gate: only the proposer withdraws. `ProposalRecord.by` is a
    //   DISPLAY hint (a WP2 re-serve re-wraps envelopes under the serving
    //   peer, and env.by is a claim) — the executing node checks `by ==
    //   self.member()` locally, and every RECEIVER checks the withdraw's
    //   authenticated wire sender against its own record/first-sighting.
    //   A mismatch drops the event (log a warn, never apply).
    // - Wire: additive `WorkspaceEvent::Withdrawn { id, by }` in
    //   `crosses_wire` (like Declined). Old readers must ignore it safely
    //   (additive-only rule); an old node simply keeps the card open.
    // - State: terminal like Rejected but its OWN state (a withdrawn card
    //   must not render as "declined by"), cleared from `pending_sigs`,
    //   parked declines dropped, and REMEMBERED (tombstone) so a WP2
    //   re-serve of the withdrawn Proposed cannot resurrect the card —
    //   same class of guard as `chain_walk.seen` in `receive_proposed`.
    // - Chain: never a block (ephemeral governance gossip, like declines).
    //   Snapshot/dump: additive fields only.
    /// Pull an OWN pending proposal back (the ProposalCard's "pull back",
    /// designed above): terminal on every node, converging like a decline
    /// — but no vote is forged. Proposer-gated twice: here against the own
    /// record, and on every receiver against ITS record + link identity.
    pub(crate) fn cmd_withdraw(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        let me = self.member();
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
            if p.by != me {
                return Err(MoltError::NotTheProposer(proposal));
            }
        }
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Withdrawn {
                id: proposal,
                by: me.clone(),
            },
        );
        self.record(env);
        self.emit(Event::Withdrawn { id: proposal });
        // the decided-vote summary, minted exactly once — on the proposer
        if let Some(payload) = self.proposals.get(&proposal.0).map(|p| p.payload.clone()) {
            let body = format!(
                "⚖ #{} ↩ {}",
                proposal.0,
                Self::decision_summary(proposal.0, &payload, None)
                    .split_once(' ')
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default()
            );
            let who = me;
            if let Err(e) = self.post_message_with_kind(
                who,
                body,
                None,
                molt_core::ChannelRef::Patch { id: proposal },
                molt_core::ChatKind::System,
            ) {
                tracing::warn!(error = %e, id = proposal.0, "could not post the withdraw summary");
            }
        }
        Ok(Reply::Ack)
    }

    /// The withdraw choke point (log applier, wire ingest, park drain) —
    /// emit-free like [`State::register_decline`]. Registers ONLY when
    /// `by` matches the record's proposer (first-sighting); a proposal
    /// this node does not know yet PARKS the verdict (bounded, one slot
    /// per id — a withdraw has exactly one legitimate author).
    pub(crate) fn register_withdraw(&mut self, id: u64, by: &str, ts: u64) -> WithdrawOutcome {
        let Some(p) = self.proposals.get_mut(&id) else {
            if id > self.next_id.saturating_add(PARKED_DECLINE_ID_WINDOW) {
                tracing::warn!(%id, %by, "dropping a withdraw for an implausible proposal id");
                return WithdrawOutcome::Ignored;
            }
            self.next_id = self.next_id.max(id.saturating_add(1));
            if !self.chain.pending_withdrawals.contains_key(&id)
                && self.chain.pending_withdrawals.len() >= PARKED_DECLINE_IDS_MAX
            {
                tracing::warn!(%id, %by, "withdraw park full - dropping");
                return WithdrawOutcome::Ignored;
            }
            self.chain.pending_withdrawals
                .entry(id)
                .or_insert((by.to_string(), ts));
            return WithdrawOutcome::Parked;
        };
        if p.state != ProposalState::Proposed {
            return WithdrawOutcome::Ignored;
        }
        if p.by != by {
            tracing::warn!(%id, %by, recorded = %p.by, "dropping a withdraw not from the recorded proposer");
            return WithdrawOutcome::Ignored;
        }
        p.state = ProposalState::Rejected;
        p.withdrawn = true;
        p.declined_at = ts; // retention + the "when" label; declined_by stays empty
        self.chain.pending_sigs.remove(&id);
        self.chain.own_approvals.remove(&id);
        self.chain.pending_declines.remove(&id);
        WithdrawOutcome::Withdrawn
    }

    /// Register a parked withdraw for a proposal that just became known.
    pub(crate) fn register_parked_withdrawal(&mut self, id: u64) -> WithdrawOutcome {
        let Some((by, ts)) = self.chain.pending_withdrawals.remove(&id) else {
            return WithdrawOutcome::Ignored;
        };
        self.register_withdraw(id, &by, ts)
    }

    pub(crate) fn cmd_decline(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        let me = self.member();
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
            if p.decliners.contains(&me) {
                return Err(MoltError::AlreadyDeclined(proposal));
            }
            // deliberately NO mirror guard against an own collected
            // signature: decline-after-approve is how a proposer WITHDRAWS
            // (the auto-cosign would otherwise lock every proposal open),
            // and the sealed-summary test pins it. The signature itself
            // still stands — retraction semantics are the D2 follow-up
            // (docs_archive/reviews/decline_convergence_review_followups.md).
        }
        // D1: the voice binds the payload the decliner SAW — a receiver
        // registers it only against a record hashing identically
        let hash = self
            .proposals
            .get(&proposal.0)
            .map(|p| Self::decline_payload_hash(&p.payload))
            .unwrap_or_default();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Declined {
                id: proposal,
                by: me.clone(),
                hash,
            },
        );
        self.record(env);
        // terminal only when the applier says so (declines > n − m — approval
        // can no longer reach the threshold); otherwise this is ONE voice
        // against, mirrored to the peers like any vote
        if self
            .proposals
            .get(&proposal.0)
            .is_some_and(|p| p.state == ProposalState::Rejected)
        {
            self.emit(Event::Rejected { id: proposal });
            // the negative decision gets its summary too, minted exactly
            // once — on the node whose decline tipped the vote terminal
            // (wire-received declines flip the state elsewhere and stay
            // silent; they receive this message instead)
            if let Some(payload) = self.proposals.get(&proposal.0).map(|p| p.payload.clone()) {
                let who = me.to_string();
                self.post_decision_summary(proposal.0, &payload, Some(&who));
            }
        } else {
            self.emit(Event::Declined {
                id: proposal,
                by: me,
            });
        }
        Ok(Reply::Ack)
    }

    /// The content anchor a decline carries (D1, `WorkspaceEvent::Declined`):
    /// lowercase sha256 hex over a domain tag + the payload's canonical
    /// JSON bytes. A CROSS-NODE contract like `molt-chat-legacy-id`:
    /// deterministic because the workspace pins serde_json's sorted-keys
    /// map (`serde_json_object_serializes_with_sorted_keys`).
    pub(crate) fn decline_payload_hash(payload: &serde_json::Value) -> String {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        h.update(b"molt-decline-payload-v1\0");
        h.update(serde_json::to_vec(payload).unwrap_or_default());
        hex::encode(h.finalize())
    }

    /// The one decline-bookkeeping choke point (log applier, wire ingest,
    /// park drain): a decline is ONE member's voice, not a veto — deduped
    /// per member, the proposal turns Rejected only when approval can no
    /// longer reach the threshold (declines > n − m). A decline for a
    /// proposal this node does not know yet PARKS (bounded) and registers
    /// when the proposal arrives: votes must never be lost to arrival
    /// order — G7 orders per sender, and a decline travels on a different
    /// sender's chain than its proposal (or replays from the own log
    /// before a re-served proposal returns). Emit-free: the applier
    /// replays through here, callers on live paths emit from the outcome.
    /// D7: would [`Self::register_decline`] SHED this wire voice on a full
    /// park? Read-only twin of its admission, consulted BEFORE the accept
    /// point — an accepted-then-shed voice was ACKed, so the at-least-once
    /// guarantee was already spent and it never comes back. Implausible-id
    /// garbage stays accept-and-drop (it must not ride resend forever),
    /// and an id already parked never sheds (the member dedup appends).
    pub(crate) fn decline_would_shed(&self, id: u64, by: &str) -> bool {
        if !self.is_chain_governed() || self.proposals.contains_key(&id) {
            return false;
        }
        if id > self.next_id.saturating_add(PARKED_DECLINE_ID_WINDOW) {
            return false; // garbage — accept and drop
        }
        if self.chain.pending_declines.contains_key(&id) {
            return false;
        }
        let member_parked = self
            .chain.pending_declines
            .values()
            .filter(|p| p.iter().any(|(m, _, _)| m == by))
            .count();
        self.chain.pending_declines.len() >= PARKED_DECLINE_IDS_MAX
            || member_parked >= PARKED_DECLINES_PER_MEMBER_MAX
    }

    pub(crate) fn register_decline(
        &mut self,
        id: u64,
        by: &str,
        ts: u64,
        hash: &str,
    ) -> DeclineOutcome {
        let is_own = by == self.member();
        let veto_room = self
            .replica
            .as_ref()
            .map(|r| r.roster.len().saturating_sub(usize::from(r.rule_m).max(1)))
            .unwrap_or(0);
        let Some(p) = self.proposals.get_mut(&id) else {
            // a real decline references an id SOME proposer minted and
            // gossiped — one absurdly far past the mint counter is garbage,
            // and letting it bump `next_id` would poison every later local
            // mint (a u64::MAX decline froze proposing for good)
            if id > self.next_id.saturating_add(PARKED_DECLINE_ID_WINDOW) {
                tracing::warn!(%id, %by, "dropping a decline for an implausible proposal id");
                return DeclineOutcome::Known;
            }
            // treat the id as taken, or a later local mint could collide
            // with the parked voice
            self.next_id = self.next_id.max(id.saturating_add(1));
            let member_parked = self
                .chain.pending_declines
                .values()
                .filter(|p| p.iter().any(|(m, _, _)| m == by))
                .count();
            if !self.chain.pending_declines.contains_key(&id)
                && (self.chain.pending_declines.len() >= PARKED_DECLINE_IDS_MAX
                    || member_parked >= PARKED_DECLINES_PER_MEMBER_MAX)
            {
                tracing::warn!(%id, %by, "decline park full - dropping");
                return DeclineOutcome::Known;
            }
            let parked = self.chain.pending_declines.entry(id).or_default();
            if !parked.iter().any(|(m, _, _)| m == by) {
                parked.push((by.to_string(), ts, hash.to_string()));
            }
            return DeclineOutcome::Parked;
        };
        // D1: a non-empty hash must match the record's payload — an empty
        // one (older sender) keeps the id-only semantics
        if !hash.is_empty() && Self::decline_payload_hash(&p.payload) != hash {
            tracing::warn!(%id, %by, "dropping a decline whose payload hash does not match this record");
            return DeclineOutcome::Known;
        }
        if p.state != ProposalState::Proposed || p.decliners.iter().any(|d| d == by) {
            return DeclineOutcome::Known;
        }
        // D2 (last vote counts, decided 2026-08-16): the decline RETRACTS
        // this member's collected signature — one member, one stance.
        // Deterministic on every caller (applier, wire, park drain, replay).
        if let Some(pending) = self.chain.pending_sigs.get_mut(&id) {
            pending.sigs.retain(|a| a.member != by);
            pending.verified.remove(by);
        }
        // …and the OWN decision register with it, or the next re-base would
        // put the retracted signature straight back (review 2026-08-25)
        if is_own {
            self.chain.own_approvals.remove(&id);
        }
        p.decliners.push(by.to_string());
        if p.decliners.len() > veto_room {
            p.state = ProposalState::Rejected;
            // envelope data only (replay determinism): when and by whom
            // (the TIPPING decliner) — the Declined read view renders both
            p.declined_at = ts;
            p.declined_by = by.to_string();
            DeclineOutcome::Rejected
        } else {
            DeclineOutcome::Voice
        }
    }

    /// Register every parked decline for a proposal that just became known.
    /// Returns the strongest outcome (Rejected > Voice > Known) so a live
    /// caller can ring frontends; replay callers ignore it. Idempotent —
    /// the park entry is consumed.
    pub(crate) fn register_parked_declines(&mut self, id: u64) -> ParkDrain {
        let mut drain = ParkDrain { voices: Vec::new(), rejected: false };
        let Some(parked) = self.chain.pending_declines.remove(&id) else {
            return drain;
        };
        for (by, ts, hash) in parked {
            match self.register_decline(id, &by, ts, &hash) {
                DeclineOutcome::Rejected => {
                    drain.voices.push(by);
                    drain.rejected = true;
                }
                DeclineOutcome::Voice => drain.voices.push(by),
                _ => {}
            }
        }
        drain
    }

    /// The one-line summary a DECIDED vote posts into its own discussion
    /// (story 2026-08-09): outcome mark, human label, the decided content,
    /// and for the negative outcome the decliner — so "Discussion" on an
    /// accepted or declined vote says what exactly was decided. English
    /// like every engine-authored notice; NEVER the image bytes.
    pub(crate) fn decision_summary(
        id: u64,
        payload: &Value,
        decliner: Option<&str>,
    ) -> String {
        let op = payload.get("op").and_then(Value::as_str).unwrap_or("");
        let label = match op {
            "set_name" => "Name",
            "set_charter" => "Charter",
            "set_chat_retention" => "Chat retention",
            "set_image" => "Logo",
            "remove_image" => "Logo removed",
            "set_relays" => "Relay pool",
            "set_features" => "Features",
            "wiki_patch" => "Wiki",
            "set_member_image" => "Member picture",
            "remove_member_image" => "Member picture removed",
            "set_member_desc" => "Member description",
            other => other,
        };
        // the decided content, capped — a charter is long, the image ops
        // carry base64 and a wiki patch carries a raw diff; none of those
        // belongs in a chat line (the card's raw-patch button shows it)
        let member = payload.get("member").and_then(Value::as_str).unwrap_or("");
        let content = match op {
            "set_image" | "remove_image" => String::new(),
            // the seat, never its picture bytes
            "set_member_image" | "remove_member_image" => member.to_string(),
            "set_member_desc" => {
                let v = payload.get("value").and_then(Value::as_str).unwrap_or("");
                let mut c: String = v.chars().take(160).collect();
                if v.chars().count() > 160 {
                    c.push('…');
                }
                format!("{member}: {c}")
            }
            "wiki_patch" => payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(160)
                .collect(),
            _ => {
                let v = payload.get("value").and_then(Value::as_str).unwrap_or("");
                let mut c: String = v.chars().take(160).collect();
                if v.chars().count() > 160 {
                    c.push('…');
                }
                c
            }
        };
        let head = if decliner.is_none() {
            format!("⚖ #{id} ✓ {label}")
        } else {
            format!("⚖ #{id} ⊘ {label}")
        };
        let mut line = if content.is_empty() {
            head
        } else {
            format!("{head}: {content}")
        };
        if let Some(who) = decliner {
            line.push_str(&format!(" · declined by {who}"));
        }
        line
    }

    /// Post a decided vote's summary into its discussion — best-effort like
    /// all chat (a failed mint must never block the decision itself).
    pub(crate) fn post_decision_summary(
        &mut self,
        id: u64,
        payload: &Value,
        decliner: Option<&str>,
    ) {
        let me = self.member();
        let body = Self::decision_summary(id, payload, decliner);
        // D5: DETERMINISTIC id — whoever tips posts, and concurrent posters
        // collapse via the ordinary duplicate-id drop. Already holding the
        // line (the other tipper's copy landed first, or a replay) = done.
        let republic = self.replica.as_ref().map(|r| r.republic_id.clone()).unwrap_or_default();
        let sid = crate::chat::decision_summary_id(&republic, id, decliner.is_some());
        if self.chat_pos.contains_key(&sid) {
            return;
        }
        if let Err(e) = self.post_message_with_kind_id(
            me,
            body,
            None,
            molt_core::ChannelRef::Patch { id: ProposalId(id) },
            molt_core::ChatKind::System,
            sid,
        ) {
            tracing::warn!(error = %e, id, "could not post the decision summary");
        }
    }

    /// Record the `Applied` event once a proposal has reached the threshold.
    /// The threshold *decision* happens here, at event-creation time; the
    /// outcome is an event of its own, so replay never re-decides it.
    fn try_apply(&mut self, id: ProposalId) {
        let ready = matches!(
            self.proposals.get(&id.0),
            Some(p) if p.state == ProposalState::Proposed
                && p.approvals >= self.threshold()
        );
        if !ready {
            return;
        }
        let me = self.member();
        let env = self.make_env(me, WorkspaceEvent::Applied { id });
        self.record(env);
        if let Some(surface) = self.proposals.get(&id.0).map(|p| p.surface) {
            self.emit(Event::Applied { id, surface });
            if surface == Surface::Organization {
                self.after_org_applied();
            }
        }
        // the decision's summary goes to ITS discussion, minted exactly
        // where the Applied event is born (the chain path posts at the
        // sealer instead — `adopt_committed_block`)
        if !self.is_chain_governed() {
            if let Some(payload) = self.proposals.get(&id.0).map(|p| p.payload.clone()) {
                self.post_decision_summary(id.0, &payload, None);
            }
        }
    }

    /// Re-decide thresholds after a replay: a crash between an `Approved`
    /// frame and its `Applied` frame must not leave a proposal stuck at
    /// `have >= need` forever. Called once per open, after the tail applied.
    ///
    /// Single-operator path only: a chain-governed workspace never applies by
    /// counting — the replayed `Approved` frames are real signatures the chain
    /// already consumed (or not), so re-counting them here would double-apply.
    pub(crate) fn recover_pending_applies(&mut self) {
        if self.is_chain_governed() {
            return;
        }
        // The honest re-decision is bounded by what the single-operator path
        // can legitimately produce: ONE real vote. A counted threshold above
        // that can only have been "met" by the removed pre-chain simulation
        // (a legacy log's invented peer approvals) — minting a fresh
        // `Applied` from such a count would fake a threshold decision no
        // member made, so those proposals stay pending (decline is the exit).
        if self.threshold() > 1 {
            return;
        }
        let ready: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.state == ProposalState::Proposed && p.approvals >= self.threshold())
            .map(|(id, _)| *id)
            .collect();
        for id in ready {
            self.try_apply(ProposalId(id));
        }
    }

    pub(crate) fn view(&self, id: u64, p: &ProposalRecord) -> ProposalView {
        // a chain-governed proposal's real progress is the count of distinct
        // collected signatures; the single-operator path counts the recorded
        // approval events (live: at most this node's own — a legacy log
        // replays what it recorded)
        let mut approvals = if self.is_chain_governed() {
            self.chain_approval_count(id)
        } else {
            p.approvals
        };
        // reader-relative: chain governance knows exactly who signed; on the
        // single-operator path the ONLY approval this node can ever record
        // is its own (self-cosign or the one explicit approve)
        let mut approved_by_me = if self.is_chain_governed() {
            let me = self.member();
            self.chain.pending_sigs
                .get(&id)
                .is_some_and(|s| s.verified.contains(&me))
        } else {
            Self::operator_approved(p)
        };
        let (mut current, proposed) = change_summary(&self.org_effective(), p);
        // a member-profile op's Ist-Stand comes from the per-member fold,
        // not from the org state (`member_profiles_plan.md` §4)
        if let Some(now) = self.member_profile_current(p) {
            current = now;
        }
        // the voting row: one stance per roster member, roster order. Chain
        // governance knows exactly who signed; the single-operator path
        // claims only what it knows — this node's own vote. (A legacy log
        // whose counter simulated peers cannot attribute them to anyone, so
        // its extra count stays anonymous rather than pinned on a member.)
        let me = self.member();
        let mut votes: Vec<MemberVote> = if self.is_chain_governed() {
            // L2: the pills show VERIFIED voices only — a raw collected
            // sig could paint a forged stance onto a named seat
            let signed: Vec<String> = self
                .chain.pending_sigs
                .get(&id)
                .map(|s| s.verified.iter().cloned().collect())
                .unwrap_or_default();
            self.roster()
                .into_iter()
                .map(|member| MemberVote {
                    vote: if signed.contains(&member) {
                        VoteState::Approved
                    } else {
                        VoteState::Open
                    },
                    member,
                })
                .collect()
        } else {
            self.roster()
                .into_iter()
                .map(|member| MemberVote {
                    vote: if member == me && Self::operator_approved(p) {
                        VoteState::Approved
                    } else {
                        VoteState::Open
                    },
                    member,
                })
                .collect()
        };
        // an APPLIED card's voters live in its sealed block — the ephemeral
        // signature collection is cleared at commit, the chain is the
        // durable record (live incident 2026-08-09, defect 7). A pruned
        // block (WP4) simply leaves the pills open: only chain-provable
        // votes are shown.
        if self.is_chain_governed() && p.state == ProposalState::Applied {
            // a Membership block carries no proposal id — match it by
            // content (op + member), like `settle_membership_records`; the
            // NEWEST matching block wins (a seat can be restored again)
            let op = p.payload.get("op").and_then(Value::as_str).unwrap_or("");
            let seat = p.payload.get("member").and_then(Value::as_str);
            // O(1) for Applied cards: the sealing sigs ride the chain
            // projection (`chain.applied_sigs`) — the per-card reverse
            // chain scan made every snapshot O(cards × chain) once ALL
            // applied history materializes cards. Only a Membership card
            // (its block carries no proposal id) still matches by content.
            let sealed = self.chain.applied_sigs.get(&id).or_else(|| {
                self.chain.blocks.iter().rev().find_map(|blk| match &blk.change {
                molt_core::ChainChange::Membership { op: bop, member, .. }
                    if seat == Some(member.as_str())
                        && matches!(
                            (op, bop),
                            ("restore_member", molt_core::MembershipOp::Restored)
                                | ("add_member", molt_core::MembershipOp::Joined)
                        ) =>
                {
                    Some(&blk.sigs)
                }
                _ => None,
                })
            });
            if let Some(sigs) = sealed {
                approvals = sigs.len();
                approved_by_me = sigs.iter().any(|a| a.member == me);
                for v in &mut votes {
                    if sigs.iter().any(|a| a.member == v.member) {
                        v.vote = VoteState::Approved;
                    }
                }
            }
            // D6: the record's stashed voter set upgrades the pills too —
            // an over-subscribed voter (truncated off the block's m sigs)
            // still reads Approved on the vote they cast. Display data,
            // per holder; `approvals` stays the chain-proven count.
            if p.voted.contains(&me) {
                approved_by_me = true;
            }
            for v in &mut votes {
                if p.voted.contains(&v.member) {
                    v.vote = VoteState::Approved;
                }
            }
        }
        // every recorded decline shows on its roster row — on a PENDING
        // proposal too (a decline is a visible voice against, not a silent
        // wait); the terminal Rejected keeps naming its tipping decliner
        // via declined_by/declined_at below
        for v in &mut votes {
            if p.decliners.contains(&v.member) {
                v.vote = VoteState::Declined;
            }
        }
        if p.state == ProposalState::Rejected && !p.declined_by.is_empty() {
            for v in &mut votes {
                if v.member == p.declined_by {
                    v.vote = VoteState::Declined;
                }
            }
        }
        ProposalView {
            id: ProposalId(id),
            surface: p.surface,
            payload: p.payload.clone(),
            approvals,
            threshold: self.threshold(),
            state: p.state,
            approved_by_me,
            declined_by_me: p.decliners.iter().any(|d| d == &me),
            current,
            proposed,
            votes,
            declined_at: p.declined_at,
            declined_by: p.declined_by.clone(),
            by: p.by.clone(),
            // reader-relative ownership (the "pull back" visibility gate);
            // "" never matches — an unknown proposer is nobody's
            mine: !p.by.is_empty() && p.by == me,
            superseded: p.superseded,
            withdrawn: p.withdrawn,
        }
    }

    /// The republic's EFFECTIVE display state: fold the applied
    /// Organization log (the applied values ARE the raw proposal payloads,
    /// last write per op wins) over the ratified genesis. Wire arrivals are
    /// not re-validated here, so the fold is defensive: an unparseable
    /// retention keeps the previous window, an empty name keeps the
    /// previous name.
    pub(crate) fn org_effective(&self) -> OrgEffective {
        let mut eff = OrgEffective {
            name: self.replica.as_ref().map(|r| r.name.clone()).unwrap_or_default(),
            agenda: self.replica.as_ref().map(|r| r.agenda.clone()).unwrap_or_default(),
            retention_days: molt_core::default_chat_retention_days(),
            image: String::new(),
            relays: self.ratified_relays().join(" "),
            // the standalone union fold (baseline ∪ applied edits) — kept
            // out of the entry loop below so gates and readers share ONE rule
            features: self.effective_features().join(" "),
        };
        // fold over the BORROWED applied entries (never clone — a `set_image`
        // entry carries the base64 image, so cloning the log here would copy
        // hundreds of KB on every read; we only read a couple of fields)
        for v in self.applied_org_entries() {
            let value = v.get("value").and_then(Value::as_str).unwrap_or_default();
            match v.get("op").and_then(Value::as_str) {
                Some("set_name") => {
                    let name = value.trim();
                    if !name.is_empty() {
                        eff.name = name.to_string();
                    }
                }
                Some("set_charter") => eff.agenda = value.to_string(),
                Some("set_chat_retention") => {
                    if let Some(days) = parse_retention_days(value) {
                        eff.retention_days = days;
                    }
                }
                Some("set_image") => {
                    // an applied set_image ALWAYS carries decodable bytes ≤
                    // the cap (validate_org_payload gates local proposals;
                    // the cmd_net_delivered guard drops undecodable/oversized
                    // peer ones), so a cheap non-empty check suffices and
                    // matches sync_logo_file — with a storage dir the
                    // reference is the materialized logo file, else (session
                    // only) the display value
                    let has_bytes =
                        v.get("bytes_b64").and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                    eff.image = match (&self.active, has_bytes) {
                        (Some(active), true) => active
                            .dir
                            .join(format!("logo.{}", logo_ext(value)))
                            .display()
                            .to_string(),
                        _ => value.to_string(),
                    };
                }
                Some("remove_image") => eff.image.clear(),
                // the shared R6 fold rule: an empty OR zero-overlap edit
                // keeps the pool (make-before-break at the fold)
                Some("set_relays") => {
                    let mut pool: Vec<String> =
                        eff.relays.split_whitespace().map(str::to_string).collect();
                    Self::fold_pool_edit(&mut pool, value);
                    eff.relays = pool.join(" ");
                }
                _ => {}
            }
        }
        eff
    }

    /// The EFFECTIVE per-member profile state: fold the applied
    /// Organization log for the member-profile ops, last write per seat and
    /// field (`member_profiles_plan.md` §4). Only members with something
    /// applied appear.
    ///
    /// Borrowed like [`State::org_effective`]'s fold — a `set_member_image`
    /// entry carries the base64 picture, so nothing here clones a payload.
    pub(crate) fn member_profiles(&self) -> std::collections::BTreeMap<&str, MemberProfile<'_>> {
        let mut out: std::collections::BTreeMap<&str, MemberProfile<'_>> =
            std::collections::BTreeMap::new();
        for v in self.applied_org_entries() {
            let Some(member) = v.get("member").and_then(Value::as_str).filter(|m| !m.is_empty())
            else {
                continue;
            };
            let op = v.get("op").and_then(Value::as_str).unwrap_or_default();
            if !matches!(op, "set_member_image" | "remove_member_image" | "set_member_desc") {
                continue;
            }
            let value = v.get("value").and_then(Value::as_str).unwrap_or_default();
            let entry = out.entry(member).or_default();
            match op {
                // the org image rule, per seat: with a storage dir the
                // reference is the materialized avatar file (what
                // `sync_avatar_files` writes), else the display value
                "set_member_image" => {
                    let has_bytes = v
                        .get("bytes_b64")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty());
                    entry.image = match (&self.active, has_bytes) {
                        (Some(active), true) => active
                            .dir
                            .join(avatar_file_name(member, value))
                            .display()
                            .to_string(),
                        _ => value.to_string(),
                    };
                }
                "remove_member_image" => entry.image.clear(),
                _ => entry.desc = value,
            }
        }
        out
    }

    /// The Ist-Stand of a member-profile proposal: the targeted seat's
    /// applied picture reference or description. `None` for every other op,
    /// so [`change_summary`]'s org rule stays the one rule.
    pub(crate) fn member_profile_current(
        &self,
        p: &molt_core::ProposalRecord,
    ) -> Option<String> {
        if p.surface != Surface::Organization || !is_member_profile_op(&p.payload) {
            return None;
        }
        let member = p.payload.get("member").and_then(Value::as_str)?;
        let profiles = self.member_profiles();
        let profile = profiles.get(member);
        Some(match p.payload.get("op").and_then(Value::as_str) {
            Some("set_member_desc") => {
                profile.map(|x| x.desc.to_string()).unwrap_or_default()
            }
            _ => profile.map(|x| x.image.clone()).unwrap_or_default(),
        })
    }

    /// The applied Organization entries, BORROWED (single-operator projection
    /// then the chain projection — one is always empty for a workspace). No
    /// clone: callers only read `op`/`value`/`bytes_b64` fields.
    pub(crate) fn applied_org_entries(&self) -> impl Iterator<Item = &Value> {
        self.applied
            .get(&Surface::Organization)
            .into_iter()
            .flatten()
            .chain(self.chain.applied.get(&Surface::Organization).into_iter().flatten())
            .map(|(_, v)| v)
    }

    /// The instant before which chat content ages out of the read contract:
    /// `now - effective retention`. A timestamp of 0 (legacy/unknown age)
    /// is always kept — unknown must not silently vanish.
    pub(crate) fn chat_retention_cutoff(&self) -> u64 {
        crate::now_secs().saturating_sub(self.org_effective().retention_days * 86_400)
    }

    /// THE retention predicate: has `ts` aged out of the read contract at
    /// `cutoff`? One definition for the bulk filter (the `None` arm of
    /// [`chat_view_admits`], behind [`State::chat_visible`]) and the point
    /// checks (download of a share, serving a fetch request), so they
    /// cannot drift. ts 0 = unknown age, never expires.
    pub(crate) fn aged_out_at(cutoff: u64, ts: u64) -> bool {
        ts != 0 && ts < cutoff
    }

    /// [`State::aged_out_at`] against the current cutoff — for single-message
    /// checks. (The bulk filter computes the cutoff once instead; this fold
    /// of the org log is too dear per message.)
    pub(crate) fn chat_ts_aged_out(&self, ts: u64) -> bool {
        Self::aged_out_at(self.chat_retention_cutoff(), ts)
    }
}

/// The retention read-filter boundary, a pure function of the message
/// timestamp, `now` and the effective window (explicit `now` so tests pin
/// it, like the `*_label_at` helpers).
///
/// ONE window, no sub-split: everything inside the retention window is
/// visible, everything past it is gone ("deleted"), and a timestamp of 0
/// (legacy/unknown age) is always kept. The window used to be halved into a
/// General and an Archive view, which meant a conversation older than half a
/// window — 3.5 days by default — silently left the chat the user was
/// looking at. Delegates to [`State::aged_out_at`], the same predicate the
/// share-expiry point checks, so the two cannot drift.
pub(crate) fn chat_view_admits(ts: u64, now: u64, retention_days: u64) -> bool {
    let cutoff = now.saturating_sub(retention_days * 86_400);
    ts == 0 || !State::aged_out_at(cutoff, ts)
}

impl State {
    /// The chat messages the read contract exposes: "delete chat after N
    /// days" is engine semantics (co-equality — GUI and MCP see the same),
    /// so a message older than the effective window is hidden from EVERY
    /// chat-derived view (the log, uploads, member upload counts, channel
    /// counts) — not just the chat pane. ts 0 (unknown age) is always kept;
    /// physical log pruning is a separate follow-up. One boundary source of
    /// truth: this is [`chat_view_admits`] with no view.
    pub(crate) fn chat_visible(&self) -> impl Iterator<Item = &molt_core::ChatMessage> {
        self.chat_visible_in(None)
    }

    /// [`State::chat_visible`] narrowed to one read slice — the contract
    /// behind `ReadState { view }`, shared by GUI and MCP (co-equality).
    ///
    /// The only slice is `"unread"` (`molt_core::CHAT_READ_SLICES`), and it
    /// is POSITION-scoped rather than time-scoped: the whole retention
    /// window, then only what sits after this channel's read cursor. Any
    /// other value — including the nav view `"today"` — is the plain window.
    pub(crate) fn chat_visible_in<'a>(
        &'a self,
        view: Option<&'a str>,
    ) -> impl Iterator<Item = &'a molt_core::ChatMessage> + 'a {
        let now = crate::now_secs();
        let days = self.org_effective().retention_days;
        let unread = view == Some("unread");
        self.chat
            .iter()
            .filter(move |m| chat_view_admits(m.ts, now, days))
            .filter(move |m| !unread || self.chat_msg_unread(m))
    }

    /// Applied log of one surface, as wire values. Chat serializes its typed
    /// messages into the same JSON shape the log always had; a `channel`
    /// filter (chat only) keeps exactly the messages filing under that
    /// channel — exact [`ChannelRef`] equality, so Topic names match by
    /// exact string (pin P3) — and a `view` filter (chat only, orthogonal)
    /// narrows to one half of the retention window ([`chat_view_admits`]).
    /// Filtered rows keep their embedded ids; position-in-`applied` is not
    /// an addressing scheme. Each value rides with the proposal id it came
    /// from (`None` = no proposal origin: chat rows, pre-id dumps) — the
    /// snapshot splits the pairs into its `applied` / `applied_ids` tracks.
    /// THE Shared-Memory base (shared_memory_real.md): the deterministic
    /// fold of the applied wiki_patch payloads in chain order over the
    /// empty founding tree. Recomputed on demand — wiki content is small
    /// text and one code path (live, replay, snapshot+tail) is what the
    /// convergence keystones pin; a cache comes only if a real history
    /// ever makes this measurable (plan decision 6).
    pub(crate) fn wiki_tree(&self) -> std::collections::BTreeMap<String, String> {
        let payloads: Vec<Value> = self
            .applied_values(Surface::Memory, None, None)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        molt_core::wiki_fold::wiki_fold(&payloads)
    }

    /// Whether a pending record's wiki patch still applies to `tree` —
    /// with THE fold's own strict apply (one function, walk == fold).
    /// Non-wiki payloads never supersede.
    fn wiki_patch_applies(
        tree: &std::collections::BTreeMap<String, String>,
        p: &ProposalRecord,
    ) -> bool {
        if p.payload.get("op").and_then(Value::as_str) != Some("wiki_patch") {
            return true;
        }
        let Some(patch) = p.payload.get("value").and_then(Value::as_str) else {
            return true; // unparseable payloads are gated at ingest
        };
        let mut clone = tree.clone();
        molt_core::wiki_fold::apply_patch(&mut clone, &molt_core::wiki_fold::parse_patch(patch))
            .is_ok()
    }

    /// The SUPERSEDE WALK (shared_memory_real.md §4): after the Memory
    /// applied projection moved — and when a proposal registers late —
    /// every pending wiki patch re-checks against the NEW base;
    /// incompatible ones transition to the terminal superseded outcome:
    /// mechanical, UNATTRIBUTED (no decline vote is forged), and
    /// deterministic on every node (a pure function of chain-ordered
    /// data), so live state, replay and snapshot+tail converge. Runs
    /// inside the deterministic apply paths; it never rings frontends —
    /// the mirror tick picks the change up.
    pub(crate) fn supersede_stale_wiki(&mut self) {
        let has_wiki_pending = self.proposals.values().any(|p| {
            p.surface == Surface::Memory
                && p.state == ProposalState::Proposed
                && p.payload.get("op").and_then(Value::as_str) == Some("wiki_patch")
        });
        if !has_wiki_pending {
            return;
        }
        let tree = self.wiki_tree();
        let stale: Vec<u64> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == Surface::Memory && p.state == ProposalState::Proposed)
            .filter(|(_, p)| !Self::wiki_patch_applies(&tree, p))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(p) = self.proposals.get_mut(&id) {
                p.state = ProposalState::Rejected;
                p.superseded = true;
            }
        }
    }

    pub(crate) fn applied_values(
        &self,
        surface: Surface,
        channel: Option<&ChannelRef>,
        view: Option<&str>,
    ) -> Vec<(Option<u64>, Value)> {
        if surface == Surface::Chat {
            self.chat_visible_in(view)
                .filter(|m| channel.map_or(true, |c| &m.channel == c))
                .map(|m| (None, serde_json::to_value(m).unwrap_or_default()))
                .collect()
        } else {
            // the surface's applied log is the single-operator projection
            // plus the chain (real threshold) projection — one of the
            // two is always empty for a given workspace, so this is a concat
            let mut v = self.applied.get(&surface).cloned().unwrap_or_default();
            if let Some(chain) = self.chain.applied.get(&surface) {
                v.extend(chain.iter().cloned());
            }
            v
        }
    }

    /// Every distinct channel in the visible chat log, one pass (chat-bus
    /// pin P7): `Group` is always listed (even when empty); the rest follow
    /// in first-appearance order, which is deterministic because the log
    /// order is canonical. Deleted (tombstoned) messages still count for
    /// their channel — they are rows in the log. Messages past the chat
    /// retention window are filtered ([`State::chat_visible`]) so the
    /// sidebar counts agree with what the read exposes.
    fn chat_channels(&self) -> Vec<ChannelInfo> {
        let mut infos = vec![ChannelInfo {
            channel: ChannelRef::Group,
            count: 0,
            last_ts: 0,
            state: None,
            unread: 0,
        }];
        let mut pos: HashMap<ChannelRef, usize> = HashMap::from([(ChannelRef::Group, 0)]);
        for m in self.chat_visible() {
            let at = *pos.entry(m.channel.clone()).or_insert_with(|| {
                infos.push(ChannelInfo {
                    channel: m.channel.clone(),
                    count: 0,
                    last_ts: 0,
                    state: None,
                    unread: 0,
                });
                infos.len() - 1
            });
            infos[at].count += 1;
            infos[at].last_ts = infos[at].last_ts.max(m.ts);
            // B2: the engine-side unread count, so GUI and MCP agree
            if self.chat_msg_unread(m) {
                infos[at].unread += 1;
            }
        }
        // annotate each patch channel with its vote's lifecycle state —
        // the read-side twin of the write guard (`ensure_channel_writable`):
        // a terminal state tells EVERY frontend (GUI and MCP alike) the
        // discussion is read-only. An unknown referent stays `None` (Q4).
        for i in &mut infos {
            if let ChannelRef::Patch { id } = &i.channel {
                i.state = self.proposals.get(&id.0).map(|p| p.state);
            }
        }
        infos
    }

    /// The read contract: the (possibly channel- and view-filtered) applied
    /// log plus, on the chat surface, the always-unfiltered channel
    /// enumeration. `view` is the retention time axis ("today"/"archive",
    /// validated by the command handler); other surfaces ignore `channel`
    /// and `view` and keep `channels` empty.
    pub(crate) fn snapshot(
        &self,
        surface: Surface,
        channel: Option<ChannelRef>,
        view: Option<&str>,
    ) -> SurfaceSnapshot {
        let pending: Vec<ProposalView> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == surface && p.state == ProposalState::Proposed)
            .map(|(id, p)| self.view(*id, p))
            .collect();
        // the declined projection (Organization → Declined): newest decline
        // first, id as the deterministic tie-breaker (the proposals map is
        // a HashMap — never lean on its iteration order). A veto ages out
        // of the view on the chat-retention rhythm (0 = unknown, kept).
        let cutoff = self.chat_retention_cutoff();
        let mut declined: Vec<ProposalView> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == surface && p.state == ProposalState::Rejected)
            .filter(|(_, p)| !Self::aged_out_at(cutoff, p.declined_at))
            .map(|(id, p)| self.view(*id, p))
            .collect();
        declined.sort_by(|a, b| b.declined_at.cmp(&a.declined_at).then(b.id.0.cmp(&a.id.0)));
        // the applied projection (Organization → Accepted): each view names
        // the voters its sealed block proves — display data the raw applied
        // payloads cannot carry. Highest id first (blocks carry no time);
        // readers join to the log's order via `applied_ids` anyway.
        let mut accepted: Vec<ProposalView> = self
            .proposals
            .iter()
            .filter(|(_, p)| p.surface == surface && p.state == ProposalState::Applied)
            .map(|(id, p)| self.view(*id, p))
            .collect();
        accepted.sort_by_key(|v| std::cmp::Reverse(v.id.0));
        let (applied_ids, applied) = self
            .applied_values(surface, channel.as_ref(), view)
            .into_iter()
            .unzip();
        // Memory serves the folded BASE with every read — the one
        // projection GUI and MCP share (shared_memory_real.md WP-B)
        let (wiki_tree, wiki_rev) = if surface == Surface::Memory {
            let payloads: Vec<Value> = self
                .applied_values(Surface::Memory, None, None)
                .into_iter()
                .map(|(_, v)| v)
                .collect();
            let (tree, rev) = molt_core::wiki_fold::wiki_fold_with_rev(&payloads);
            (
                tree.into_iter()
                    .map(|(path, content)| molt_core::WikiDoc { path, content })
                    .collect(),
                rev,
            )
        } else {
            (Vec::new(), 0)
        };
        SurfaceSnapshot {
            surface,
            gated: surface.is_gated(),
            applied,
            applied_ids,
            pending,
            denied: declined.len(),
            declined,
            accepted,
            channels: if surface == Surface::Chat {
                self.chat_channels()
            } else {
                Vec::new()
            },
            wiki_tree,
            wiki_rev,
            // the chat is one window now — nothing is filed away, so there
            // is no second view to offer or hide. Kept on the wire (always
            // false) rather than removed, so an older reader that still asks
            // is told "no archive" instead of failing to decode.
            has_archive: false,
        }
    }

    /// Whether a pending proposal still awaits `member`'s approval. Chain
    /// governance knows exactly who signed; on the single-operator path
    /// the only recordable approval is the local member's own, so a peer's
    /// approval is honestly always still outstanding — nothing can ever
    /// deliver it in a context without chain governance.
    pub(crate) fn waits_on(&self, id: u64, p: &ProposalRecord, member: &str) -> bool {
        if p.state != ProposalState::Proposed {
            return false;
        }
        // a member who DECLINED has voted: the card stays open until the
        // declines can outvote the threshold, but it awaits nobody's voice
        // twice. Without this the members table counts a decided seat as
        // pending forever, and the wake hook nudges an agent on every open
        // for work it already answered.
        if p.decliners.iter().any(|d| d == member) {
            return false;
        }
        if self.is_chain_governed() {
            !self
                .chain.pending_sigs
                .get(&id)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == member))
        } else if member == self.member() {
            !Self::operator_approved(p)
        } else {
            true
        }
    }

    /// The Organization → Members table: one row per roster member. The
    /// identity anchor comes from the genesis (real on ritual-founded
    /// workspaces); presence is the REAL last-seen stamp, aged live at
    /// read time (a send-failure pin wins) — prose is rendered UI-side.
    pub(crate) fn members_view(&self) -> Vec<MemberView> {
        let now = self.presence_now();
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let splits = self.relay_splits();
        let profiles = self.member_profiles();
        self.roster()
            .into_iter()
            .map(|member| {
                let identity_pk = self
                    .replica
                    .as_ref()
                    .and_then(|r| r.identities.iter().find(|i| i.member == member))
                    .map(|i| i.identity_pk.clone())
                    .unwrap_or_default();
                // a human-scale fingerprint: the key's leading 16 hex chars
                let id = identity_pk.get(..16).unwrap_or_default().to_string();
                let last_seen = entry
                    .and_then(|e| e.members.iter().find(|m| m.name == member))
                    .map(|m| m.last_seen)
                    .unwrap_or(molt_core::MemberInfo::NEVER);
                let presence = self.presence_of(&member, last_seen, now);
                // R4: the split marker — one compact line naming the
                // counterpart(s) and this member's own first relay (the one
                // the others would have to add to bridge)
                let others: Vec<&str> = splits
                    .iter()
                    .filter_map(|(a, b)| match (&member == a, &member == b) {
                        (true, _) => Some(b.as_str()),
                        (_, true) => Some(a.as_str()),
                        _ => None,
                    })
                    .collect();
                let split = if others.is_empty() {
                    String::new()
                } else {
                    let own = self.member_relays(&member).first().cloned().unwrap_or_default();
                    format!("no shared relay with {} ({own})", others.join(", "))
                };
                let profile = profiles.get(member.as_str());
                MemberView {
                    image: profile.map(|p| p.image.clone()).unwrap_or_default(),
                    description: profile.map(|p| p.desc.to_string()).unwrap_or_default(),
                    open_proposals: self
                        .proposals
                        .iter()
                        .filter(|(pid, p)| self.waits_on(**pid, p, &member))
                        .count(),
                    uploads: self
                        .chat_visible()
                        .filter(|m| m.from == member && m.file.is_some())
                        .count(),
                    split,
                    member,
                    id,
                    identity_pk,
                    last_seen,
                    presence,
                }
            })
            .collect()
    }

    /// The Organization → Uploads table: every file shared into the chat
    /// (within the retention window — [`State::chat_visible`]), in log order.
    /// Only metadata — the bytes move user-to-user over a dedicated encrypted
    /// queue, which is why a download needs the sharer online; the checksum
    /// is the real sha256. Uploads are ephemeral exactly like chat:
    /// `expires_ts` is the REAL retention deadline (`ts` + the org's
    /// `retention_days` — the one knob chat filters on; 0 = unknown age,
    /// no deadline), past which the share leaves every read surface.
    pub(crate) fn uploads_view(&self) -> Vec<UploadView> {
        let retention_secs = self.org_effective().retention_days * 86_400;
        let me = self.member();
        let now = self.presence_now();
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        // "sharer online?" from the REAL stamps, aged at read time (self is
        // always online, a send-failure pin wins) — a never-seen sharer is
        // honestly offline
        let presence = |member: &str| {
            let last_seen = entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.last_seen)
                .unwrap_or(molt_core::MemberInfo::NEVER);
            self.presence_of(member, last_seen, now)
        };
        self.chat_visible()
            .filter_map(|m| {
                m.file.as_ref().map(|f| UploadView {
                    id: m.id,
                    member: m.from.clone(),
                    ts: m.ts,
                    name: f.name.clone(),
                    kind: f.kind.clone(),
                    size: f.size,
                    available: f.available,
                    expires_ts: if m.ts == 0 { 0 } else { m.ts.saturating_add(retention_secs) },
                    online: m.from == me || presence(&m.from) != 2,
                    // the sharer's log-anchored sha256 ("" = legacy share,
                    // honestly unknown) — what a download must reproduce
                    checksum: f.checksum.clone(),
                    download: self.files.downloads.get(&m.id).cloned(),
                    // §5.5: ONE derived word, no extra state — a live stamp
                    // outranks `available` (relay copies outlive a removal
                    // until retention prunes them)
                    availability: if self.files.series.contains_key(&m.id) {
                        "relay-held"
                    } else if f.available {
                        "sharer-only"
                    } else {
                        "gone"
                    }
                    .to_string(),
                })
            })
            .collect()
    }

    pub(crate) fn status(&self) -> StatusView {
        // the activity trio counts REAL last-seen stamps per window; the
        // local member always counts (it is the one asking), a never-seen
        // member counts nowhere
        let now = self.presence_now();
        let me = self.member();
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let active_within = |member: &str, window_secs: u64| {
            if member == me {
                return true;
            }
            let last_seen = entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.last_seen)
                .unwrap_or(molt_core::MemberInfo::NEVER);
            last_seen != molt_core::MemberInfo::NEVER
                && now.saturating_sub(last_seen) <= window_secs
        };
        let roster = self.roster();
        let active_1h = roster.iter().filter(|m| active_within(m, 3_600)).count();
        let active_24h = roster.iter().filter(|m| active_within(m, 86_400)).count();
        let active_7d = roster.iter().filter(|m| active_within(m, 604_800)).count();
        let surfaces = Surface::ALL
            .into_iter()
            .map(|s| {
                let pending = self
                    .proposals
                    .values()
                    .filter(|p| p.surface == s && p.state == ProposalState::Proposed)
                    .count();
                let applied = if s == Surface::Chat {
                    // count what the read contract shows (retention window)
                    self.chat_visible().count()
                } else {
                    self.applied.get(&s).map(|v| v.len()).unwrap_or(0)
                        + self.chain.applied.get(&s).map(|v| v.len()).unwrap_or(0)
                };
                SurfaceStat {
                    surface: s,
                    gated: s.is_gated(),
                    applied,
                    pending,
                }
            })
            .collect();
        let eff = self.org_effective();
        StatusView {
            member: self.member(),
            members: roster,
            threshold: self.threshold(),
            surfaces,
            founded_ts: self.replica.as_ref().map(|r| r.founded_ts).unwrap_or(0),
            active_1h,
            active_24h,
            active_7d,
            image: eff.image,
            name: eff.name,
            agenda: eff.agenda,
            chat_retention_days: eff.retention_days,
            // the group's pool, from the live transport material — NOT this
            // node's own settings pool, which is a different list (what I can
            // dial vs. what the group agreed on)
            relays: self
                .nostr
                .as_ref()
                .map(|n| n.relays.clone())
                .unwrap_or_default(),
            // recovery exists exactly here — the frontends key the per-member
            // "recovery link" action on this (never on the member's presence:
            // a recovery link is FOR an unreachable member)
            chain_governed: self.is_chain_governed(),
            features: self.effective_features(),
            // the honest downscale target a frontend fits a picture to
            // before proposing — this republic's own derived headroom
            image_budget: u64::try_from(member_image_budget(&me, &self.roster())).unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod size_gate_tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    /// L10: the retention pair carries MACHINE values — the unit renders
    /// in the frontends, per language ("7 days → 30 days" leaked English
    /// into the German card, and the value string is display-only).
    #[test]
    fn the_retention_pair_carries_machine_values_not_english() {
        let eff = OrgEffective {
            name: String::new(),
            agenda: String::new(),
            retention_days: 7,
            image: String::new(),
            relays: String::new(),
            features: String::new(),
        };
        let rec = molt_core::ProposalRecord {
            surface: molt_core::Surface::Organization,
            payload: json!({ "op": "set_chat_retention", "value": "30" }),
            approvals: 0,
            state: molt_core::ProposalState::Proposed,
            declined_at: 0,
            declined_by: String::new(),
            decliners: Vec::new(),
            voted: Vec::new(),
            by: String::new(),
            superseded: false,
            withdrawn: false,
        };
        let (current, proposed) = change_summary(&eff, &rec);
        assert_eq!((current.as_str(), proposed.as_str()), ("7", "30"));
    }

    fn roster() -> Vec<molt_core::MemberId> {
        ["walter", "petra", "hannelore-von-und-zu"]
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// A decodable image of exactly `len` bytes: a 2x2 BMP header padded out.
    /// The sniff reads only the header, so the padding rides free — which is
    /// what makes an arbitrary SIZE testable without shipping a fixture.
    fn padded_bmp(len: usize) -> Vec<u8> {
        let mut b = crate::tests::tiny_bmp_header(2, 2);
        b.resize(len.max(b.len()), 0x00);
        b
    }

    fn image_payload(bytes: &[u8]) -> Value {
        json!({
            "op": "set_image",
            "title": "t",
            "value": "logo.png",
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// **The keystone: the gate and the transport agree.**
    ///
    /// The largest payload the propose path accepts must frame inside the
    /// budget `RelayRuntime::publish` enforces — measured through the same
    /// cost model `molt-net/tests/group_frame_budget.rs` pins against the
    /// real pipeline. This is the assertion whose absence let a 256 KiB cap
    /// sit in front of a 128 KiB transport for as long as it did.
    #[test]
    fn the_largest_accepted_payload_frames_inside_the_transport_budget() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &image_payload(&[]), &roster);
        let payload = image_payload(&padded_bmp(headroom));

        assert!(
            payload_fits(Surface::Organization, &payload, &roster),
            "the advertised headroom of {headroom} B is not itself accepted"
        );
        let plaintext = applied_block_plaintext_len(Surface::Organization, &payload, &roster);
        let cost = u64::try_from(molt_net::envelope::frame_cost(plaintext)).expect("cost fits");
        assert!(
            cost <= molt_net::relay_runtime::DEFAULT_SIZE_BUDGET,
            "a payload at the accepted ceiling frames to {cost} B, over the \
             {} B publish budget - every proposal at the cap would wedge the outbox",
            molt_net::relay_runtime::DEFAULT_SIZE_BUDGET
        );
    }

    /// …and the headroom is the REAL edge, not a number with slack behind it:
    /// three more decoded bytes are one base64 quantum too many.
    #[test]
    fn one_quantum_above_the_headroom_is_refused() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &image_payload(&[]), &roster);
        let payload = image_payload(&padded_bmp(headroom + 3));
        assert!(
            !payload_fits(Surface::Organization, &payload, &roster),
            "the headroom leaves slack, so it is not the derived edge"
        );
    }

    /// The refusal names the number to aim at, and names it in the unit the
    /// human picked the file in.
    #[test]
    fn the_refusal_names_the_size_that_would_fit() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &image_payload(&[]), &roster);
        let payload = image_payload(&padded_bmp(headroom * 2));
        let err = validate_payload_fits(Surface::Organization, &payload, &roster)
            .expect_err("an oversized image is refused");
        let MoltError::BadPayload(msg) = err else {
            panic!("expected BadPayload");
        };
        assert!(
            msg.contains(&format!("{} KiB", headroom / 1024)),
            "the refusal does not say what fits: {msg}"
        );
    }

    /// **The gate is not about images.** A charter long enough to blow the
    /// same budget is refused by the same rule, with a message that does not
    /// mislead the reader into looking for a picture.
    #[test]
    fn an_over_long_charter_is_refused_by_the_same_rule() {
        let roster = roster();
        let payload = json!({
            "op": "set_charter",
            "value": "x".repeat(transport_plaintext_ceiling() + 1),
        });
        let err = validate_payload_fits(Surface::Organization, &payload, &roster)
            .expect_err("an over-long charter is refused");
        let MoltError::BadPayload(msg) = err else {
            panic!("expected BadPayload");
        };
        assert!(
            !msg.contains("image"),
            "a charter refusal must not send the reader hunting for an image: {msg}"
        );
    }

    /// The regression this whole change exists for: the accepted ceiling is
    /// FAR below the 256 KiB that used to be allowed, and still large enough
    /// for a real logo — so the answer was "derive the cap", not "images
    /// cannot ride the chain".
    #[test]
    fn the_derived_ceiling_replaces_the_chosen_one() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &image_payload(&[]), &roster);
        eprintln!("MEASURED image headroom: {} KiB", headroom / 1024);
        assert!(
            headroom < 256 * 1024 / 2,
            "the derived headroom {headroom} B is not meaningfully below the 256 KiB \
             that was there before - nothing was reconciled"
        );
        assert!(
            headroom >= 32 * 1024,
            "only {headroom} B fits - too small for a logo; the answer would have to \
             be structural (a hash in the block, bytes elsewhere)"
        );
    }

    /// A bigger republic pays for its own seats: every seat's attestation
    /// rides the block, so the headroom shrinks as `n` grows. It must shrink
    /// — a ceiling blind to the roster is the same class of mistake again.
    #[test]
    fn a_larger_roster_leaves_less_room() {
        let small = image_headroom(Surface::Organization, &image_payload(&[]), &roster());
        let big: Vec<molt_core::MemberId> =
            (0..50).map(|i| format!("member-number-{i:03}")).collect();
        let large = image_headroom(Surface::Organization, &image_payload(&[]), &big);
        assert!(
            large < small,
            "50 seats ({large} B) left as much room as 3 ({small} B) - the block's \
             own signatures are not being counted"
        );
    }

    // ---- member profiles (`member_profiles_plan.md` §2) ----------------

    fn member_image_payload(bytes: &[u8]) -> Value {
        json!({
            "op": "set_member_image",
            "member": "walter",
            "value": "face.png",
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// A profile picture is gated by the SAME derived ceiling as the org
    /// logo — no second, chosen cap — and the headroom is the real edge.
    #[test]
    fn a_member_image_rides_the_same_derived_ceiling() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &member_image_payload(&[]), &roster);
        assert!(
            payload_fits(
                Surface::Organization,
                &member_image_payload(&padded_bmp(headroom)),
                &roster
            ),
            "the advertised headroom of {headroom} B is not itself accepted"
        );
        assert!(
            !payload_fits(
                Surface::Organization,
                &member_image_payload(&padded_bmp(headroom + 3)),
                &roster
            ),
            "the headroom leaves slack, so it is not the derived edge"
        );
    }

    /// …and an oversized avatar's refusal names the IMAGE and the size that
    /// would fit, like the org logo's does.
    #[test]
    fn the_member_image_refusal_names_the_size_that_would_fit() {
        let roster = roster();
        let headroom = image_headroom(Surface::Organization, &member_image_payload(&[]), &roster);
        let payload = member_image_payload(&padded_bmp(headroom * 2));
        let err = validate_payload_fits(Surface::Organization, &payload, &roster)
            .expect_err("an oversized avatar is refused");
        let MoltError::BadPayload(msg) = err else {
            panic!("expected BadPayload");
        };
        assert!(msg.contains("image"), "the refusal must name the image: {msg}");
        assert!(
            msg.contains(&format!("{} KiB", headroom / 1024)),
            "the refusal does not say what fits: {msg}"
        );
    }

    /// A profile picture is rendered in a square box on every frontend, so
    /// the engine refuses a non-square one (h) — naming the dimensions it
    /// actually got. The org logo stays unconstrained.
    #[test]
    fn a_member_image_must_be_square() {
        let mut wide = crate::tests::tiny_bmp_header(4, 2);
        wide.resize(wide.len() + 8, 0);
        let err = validate_org_payload(Surface::Organization, &member_image_payload(&wide))
            .expect_err("a non-square avatar is refused");
        let MoltError::BadPayload(msg) = err else {
            panic!("expected BadPayload");
        };
        assert!(msg.contains("4x2"), "the refusal must name the dimensions: {msg}");
        // …the square twin passes, and the org logo keeps taking any shape
        validate_org_payload(Surface::Organization, &member_image_payload(&padded_bmp(64)))
            .expect("a square avatar is accepted");
        let org = json!({
            "op": "set_image",
            "value": "logo.png",
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(&wide),
        });
        validate_org_payload(Surface::Organization, &org)
            .expect("the org logo has no square rule");
    }

    /// Sign-what-you-see: an avatar proposal without the bytes is refused.
    #[test]
    fn a_member_image_must_embed_its_bytes() {
        let payload = json!({ "op": "set_member_image", "member": "walter", "value": "face.png" });
        validate_org_payload(Surface::Organization, &payload)
            .expect_err("an avatar proposal without bytes is refused");
    }

    /// A table column is not a charter: the description is capped, and the
    /// refusal names the limit. An empty value clears it.
    #[test]
    fn a_long_description_is_refused_naming_the_limit() {
        let desc = |v: String| json!({ "op": "set_member_desc", "member": "walter", "value": v });
        validate_org_payload(Surface::Organization, &desc("x".repeat(DESC_MAX)))
            .expect("the limit itself is accepted");
        validate_org_payload(Surface::Organization, &desc(String::new()))
            .expect("an empty description clears it");
        let err = validate_org_payload(Surface::Organization, &desc("x".repeat(DESC_MAX + 1)))
            .expect_err("one character over the limit is refused");
        let MoltError::BadPayload(msg) = err else {
            panic!("expected BadPayload");
        };
        assert!(
            msg.contains(&DESC_MAX.to_string()),
            "the refusal does not name the limit: {msg}"
        );
        // the cap counts CHARACTERS, not bytes
        validate_org_payload(Surface::Organization, &desc("ä".repeat(DESC_MAX)))
            .expect("a multi-byte description is measured in characters");
    }

    /// The avatar file name is derived from a member-controlled string, so
    /// it must stay a NAME: no separator, no traversal, and two seats whose
    /// slugs collide still get their own file.
    #[test]
    fn the_avatar_file_name_stays_a_path_safe_name() {
        let name = avatar_file_name("../../etc/pas swd", "x.PNG");
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert!(name.starts_with("avatar-") && name.ends_with(".png"), "{name}");
        assert_ne!(
            avatar_stem("Anna B"),
            avatar_stem("anna-b"),
            "colliding slugs must not share a file"
        );
        // The stem has to satisfy molt-storage's file-name guard for EVERY
        // roster name, or that seat's picture fails to materialize on every
        // node, forever. Turkish dotted I is the trap: `to_lowercase()`
        // expands U+0130 into "i" + U+0307, a combining MARK that the guard
        // refuses - and the slug happily carried it.
        for member in ["İhsan", "北京", "Anna B", "../../etc/pas swd", "-- --"] {
            let stem = avatar_stem(member);
            assert!(
                !stem.is_empty() && stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "storage refuses this avatar name for {member:?}: {stem:?}"
            );
        }
    }

    /// The served budget is a PROMISE: an image fitted to it is accepted,
    /// file name included — that is what the reserved name allowance buys.
    #[test]
    fn what_the_served_image_budget_promises_is_accepted() {
        let roster = roster();
        let budget = member_image_budget("hannelore-von-und-zu", &roster);
        assert!(budget >= 32 * 1024, "the served budget is unusably small: {budget} B");
        let payload = json!({
            "op": "set_member_image",
            "member": "hannelore-von-und-zu",
            "value": "x".repeat(AVATAR_NAME_ALLOWANCE),
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(padded_bmp(budget)),
        });
        assert!(
            payload_fits(Surface::Organization, &payload, &roster),
            "the served budget of {budget} B is not itself accepted"
        );
    }

    /// The decided-vote chat line names the seat, never the picture bytes
    /// (the 987 rule: base64 does not belong in a chat line).
    #[test]
    fn the_decision_line_names_the_seat_never_the_bytes() {
        let img = json!({
            "op": "set_member_image",
            "member": "walter",
            "value": "face.png",
            "bytes_b64": "QUJDREVGRw==",
        });
        let line = State::decision_summary(7, &img, None);
        assert!(line.contains("Member picture"), "{line}");
        assert!(line.contains("walter"), "{line}");
        assert!(!line.contains("QUJD"), "the image bytes leaked into a chat line: {line}");

        let removed = json!({ "op": "remove_member_image", "member": "walter" });
        let line = State::decision_summary(8, &removed, None);
        assert!(line.contains("Member picture removed"), "{line}");
        assert!(line.contains("walter"), "{line}");

        let desc = json!({ "op": "set_member_desc", "member": "walter", "value": "keeps the bees" });
        let line = State::decision_summary(9, &desc, None);
        assert!(line.contains("Member description"), "{line}");
        assert!(line.contains("walter: keeps the bees"), "{line}");
    }

    /// **Self-edit, local**: a profile op is refused unless it names THIS
    /// node's own seat, and the seat must be in the roster.
    #[test]
    fn a_profile_op_for_another_member_is_refused() {
        let mut st = crate::tests::plain_state();
        let op = |m: &str| json!({ "op": "set_member_desc", "member": m, "value": "hi" });
        let err = st
            .cmd_propose(Surface::Organization, op("peer-1"))
            .expect_err("editing another member's profile is refused");
        assert_eq!(format!("{err}"), "bad payload: only peer-1 can edit this profile");
        let err = st
            .cmd_propose(Surface::Organization, op("ghost"))
            .expect_err("a profile op for a non-seat is refused");
        assert_eq!(format!("{err}"), "bad payload: ghost: not in the roster");
        let err = st
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_member_desc", "value": "hi" }),
            )
            .expect_err("a profile op without a member is refused");
        assert_eq!(format!("{err}"), "bad payload: the profile op needs a member");
        st.cmd_propose(Surface::Organization, op("me"))
            .expect("a member may edit its own profile");
    }

    /// One spelling per description: the stored value is trimmed, so
    /// whitespace-only input clears instead of rendering blanks (the
    /// set_relays/set_features canonicalization precedent).
    #[test]
    fn a_proposed_description_is_stored_trimmed() {
        let mut st = crate::tests::plain_state();
        let Reply::Proposed { id } = st
            .cmd_propose(
                Surface::Organization,
                json!({ "op": "set_member_desc", "member": "me", "value": "  keeps the bees \n" }),
            )
            .expect("propose")
        else {
            panic!("expected Proposed");
        };
        let stored = st.proposals.get(&id.0).expect("recorded");
        assert_eq!(
            stored.payload.get("value").and_then(Value::as_str),
            Some("keeps the bees")
        );
    }
}
