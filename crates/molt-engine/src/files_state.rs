// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared Files votes (`docs_archive/files/persistent_uploads.md` D1/D2):
//! the fold of the persist/unpersist log, the ONE expiry rule every share
//! consumer reads, and the two doors a Files proposal passes - propose
//! (the identity is written here) and approve (it is checked here).

use std::collections::HashMap;

use molt_core::{MessageId, MoltError, ProposalState, Surface};
use serde_json::Value;

use crate::State;

/// How far an `unpersist` stamp may sit from a seat's clock.
const UNPERSIST_SKEW: u64 = 3_600;

/// A share's identity as the persist block carries it - the chat message
/// may be gone by the time anyone reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShareIdentity {
    pub(crate) by: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) size: u64,
    pub(crate) checksum: String,
    pub(crate) shared_ts: u64,
}

impl ShareIdentity {
    fn from_payload(v: &Value) -> Option<ShareIdentity> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let me = ShareIdentity {
            by: s("by")?,
            name: s("name")?,
            kind: s("kind").unwrap_or_default(),
            size: v.get("size").and_then(Value::as_u64)?,
            checksum: s("checksum").unwrap_or_default(),
            shared_ts: v.get("shared_ts").and_then(Value::as_u64).unwrap_or(0),
        };
        (!me.by.is_empty() && !me.name.is_empty()).then_some(me)
    }

    fn write_into(&self, v: &mut Value) {
        v["by"] = Value::from(self.by.as_str());
        v["name"] = Value::from(self.name.as_str());
        v["kind"] = Value::from(self.kind.as_str());
        v["size"] = Value::from(self.size);
        v["checksum"] = Value::from(self.checksum.as_str());
        v["shared_ts"] = Value::from(self.shared_ts);
    }
}

/// What the votes say about one share: last op per id wins.
#[derive(Clone, Debug)]
pub(crate) enum FileState {
    Persistent(ShareIdentity),
    /// Back to temporary since `at` - its window restarts there.
    Unpersisted(ShareIdentity, u64),
}

impl FileState {
    pub(crate) fn identity(&self) -> &ShareIdentity {
        match self {
            FileState::Persistent(m) | FileState::Unpersisted(m, _) => m,
        }
    }
}

pub(crate) type FilesState = HashMap<MessageId, FileState>;

fn parse_id(v: &Value) -> Option<MessageId> {
    v.get("id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<MessageId>().ok())
}

/// The node-independent shape check - the wire door and the propose door
/// alike, so peers agree on what to drop. Both ops carry the identity: a
/// checkpoint keeps only the LATEST op per share (`applied_lww_slot`), so
/// an unpersist must fold on its own.
pub(crate) fn validate_files_payload(v: &Value) -> Result<(), MoltError> {
    let bad = |m: &str| MoltError::BadPayload(m.to_string());
    let op = v.get("op").and_then(Value::as_str).unwrap_or("");
    if !matches!(op, "persist" | "unpersist") {
        return Err(bad("files ops: persist {id} · unpersist {id, at}"));
    }
    if parse_id(v).is_none() {
        return Err(bad("a share id (32 hex chars) is required"));
    }
    if ShareIdentity::from_payload(v).is_none() {
        return Err(bad("the share's identity (by, name, size) is missing"));
    }
    if op == "unpersist" && v.get("at").and_then(Value::as_u64).is_none() {
        return Err(bad("unpersist needs its stamp `at`"));
    }
    Ok(())
}

impl State {
    /// The Files applied log, borrowed (the legacy projection and the chain
    /// projection - one of them is always empty for a workspace).
    pub(crate) fn applied_files_entries(&self) -> impl Iterator<Item = &Value> {
        self.applied
            .get(&Surface::Files)
            .into_iter()
            .flatten()
            .chain(self.chain.applied.get(&Surface::Files).into_iter().flatten())
            .map(|(_, v)| v)
    }

    /// The fold of the Files applied log. Deterministic from the log alone,
    /// so recovery, catch-up and a checkpoint-seeded holder rebuild it.
    pub(crate) fn files_state(&self) -> FilesState {
        let mut out = FilesState::new();
        for v in self.applied_files_entries() {
            let Some(id) = parse_id(v) else {
                continue;
            };
            match v.get("op").and_then(Value::as_str) {
                Some("persist") => {
                    if let Some(meta) = ShareIdentity::from_payload(v) {
                        out.insert(id, FileState::Persistent(meta));
                    }
                }
                // the identity rides the unpersist too (a cut keeps only
                // the latest op per share); the prior persist is the
                // fallback for a block that predates that rule
                Some("unpersist") => {
                    let Some(at) = v.get("at").and_then(Value::as_u64) else {
                        continue;
                    };
                    let meta = ShareIdentity::from_payload(v)
                        .or_else(|| out.get(&id).map(|prev| prev.identity().clone()));
                    if let Some(meta) = meta {
                        // the window never restarts before the share existed
                        let at = at.max(meta.shared_ts);
                        out.insert(id, FileState::Unpersisted(meta, at));
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub(crate) fn is_persistent_share(&self, id: &MessageId) -> bool {
        matches!(self.files_state().get(id), Some(FileState::Persistent(_)))
    }

    /// A share's identity and availability: the live message, else the
    /// persist block once the message left the log (or lost its file to a
    /// tombstone). For the block's word on availability: a foreign share
    /// is what the vote said it is, an own share is available while this
    /// node still knows its path.
    pub(crate) fn share_identity(&self, id: &MessageId) -> Result<(ShareIdentity, bool), MoltError> {
        let live = self.chat_by_id(id).ok().and_then(|(_, msg)| {
            msg.file.as_ref().map(|f| {
                (
                    ShareIdentity {
                        by: msg.from.clone(),
                        name: f.name.clone(),
                        kind: f.kind.clone(),
                        size: f.size,
                        checksum: f.checksum.clone(),
                        shared_ts: msg.ts,
                    },
                    f.available,
                )
            })
        });
        if let Some(found) = live {
            return Ok(found);
        }
        match self.files_state().get(id) {
            Some(st) => {
                let meta = st.identity().clone();
                let available = meta.by != self.member() || self.files.share_paths.contains_key(id);
                Ok((meta, available))
            }
            None => match self.chat_by_id(id) {
                Ok(_) => Err(MoltError::NoFile(*id)),
                Err(e) => Err(e),
            },
        }
    }

    fn retention_secs(&self) -> u64 {
        self.org_effective().retention_days * 86_400
    }

    /// When a share leaves the tables and the download gates: `None` =
    /// never (persistent), `Some(0)` = unknown age (kept), else the unix
    /// deadline - the chat window for a plain share, the unpersist stamp's
    /// window for an unpersisted one. `states` is one [`State::files_state`]
    /// fold, so a caller with many shares folds once.
    pub(crate) fn share_expiry_in(&self, states: &FilesState, id: &MessageId) -> Option<u64> {
        match states.get(id) {
            Some(FileState::Persistent(_)) => None,
            Some(FileState::Unpersisted(_, at)) => Some(at.saturating_add(self.retention_secs())),
            None => {
                let ts = self.chat_by_id(id).map(|(_, m)| m.ts).unwrap_or(0);
                Some(if ts == 0 { 0 } else { ts.saturating_add(self.retention_secs()) })
            }
        }
    }

    pub(crate) fn share_expired_in(&self, states: &FilesState, id: &MessageId) -> bool {
        match self.share_expiry_in(states, id) {
            None | Some(0) => false,
            Some(deadline) => deadline < crate::now_secs(),
        }
    }

    pub(crate) fn share_expired(&self, id: &MessageId) -> bool {
        self.share_expired_in(&self.files_state(), id)
    }

    fn open_files_vote(&self, op: &str, id_hex: &str) -> bool {
        self.proposals.values().any(|p| {
            p.surface == Surface::Files
                && p.state == ProposalState::Proposed
                && p.payload.get("op").and_then(Value::as_str) == Some(op)
                && p.payload.get("id").and_then(Value::as_str) == Some(id_hex)
        })
    }

    /// The propose door: refuse what the tables cannot take, and write the
    /// share's identity into the payload (sign-what-you-see - whatever the
    /// proposer sent for those fields).
    pub(crate) fn prepare_files_proposal(&self, payload: &mut Value) -> Result<(), MoltError> {
        let bad = |m: &str| MoltError::BadPayload(m.to_string());
        let op = payload.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        let id_hex = payload.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let id: MessageId = id_hex
            .parse()
            .map_err(|_| bad("a share id (32 hex chars) is required"))?;
        let states = self.files_state();
        match op.as_str() {
            "persist" => {
                if matches!(states.get(&id), Some(FileState::Persistent(_))) {
                    return Err(bad("already persistent"));
                }
                if self.open_files_vote("persist", &id_hex) {
                    return Err(bad("a persist vote for this share is open"));
                }
                let (ident, available) =
                    self.share_identity(&id).map_err(|_| bad("not a shared file"))?;
                if !available {
                    return Err(MoltError::FileUnavailable(id));
                }
                if self.share_expired_in(&states, &id) {
                    return Err(MoltError::FileExpired(id));
                }
                ident.write_into(payload);
            }
            "unpersist" => {
                let Some(FileState::Persistent(meta)) = states.get(&id) else {
                    return Err(bad("not persistent"));
                };
                if self.open_files_vote("unpersist", &id_hex) {
                    return Err(bad("an unpersist vote for this share is open"));
                }
                let at = payload
                    .get("at")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| bad("unpersist needs its stamp `at`"))?;
                if at.abs_diff(crate::now_secs()) > UNPERSIST_SKEW {
                    return Err(bad("`at` is off this clock by more than an hour"));
                }
                if at < meta.shared_ts {
                    return Err(bad("`at` lies before the share"));
                }
                meta.write_into(payload);
            }
            _ => return Err(bad("files ops: persist {id} · unpersist {id, at}")),
        }
        validate_files_payload(payload)
    }

    /// The approve door: a peer's proposal must name the file THIS seat
    /// knows under that id, and its stamp must be one this seat could have
    /// proposed - no signature for a foreign identity or a stamp that
    /// expires a share on arrival.
    pub(crate) fn check_files_vote(&self, payload: &Value) -> Result<(), MoltError> {
        let bad = |m: &str| MoltError::BadPayload(m.to_string());
        validate_files_payload(payload)?;
        let id = parse_id(payload).ok_or_else(|| bad("a share id is required"))?;
        let claimed = ShareIdentity::from_payload(payload).ok_or_else(|| bad("no identity"))?;
        let states = self.files_state();
        match payload.get("op").and_then(Value::as_str).unwrap_or("") {
            "persist" => {
                if matches!(states.get(&id), Some(FileState::Persistent(_))) {
                    return Err(bad("already persistent"));
                }
                let (mine, available) =
                    self.share_identity(&id).map_err(|_| bad("not a shared file here"))?;
                if !available {
                    return Err(MoltError::FileUnavailable(id));
                }
                if self.share_expired_in(&states, &id) {
                    return Err(MoltError::FileExpired(id));
                }
                if mine != claimed {
                    return Err(bad("the proposal names a different file than this seat has"));
                }
            }
            _ => {
                let Some(FileState::Persistent(meta)) = states.get(&id) else {
                    return Err(bad("not persistent"));
                };
                if *meta != claimed {
                    return Err(bad("the proposal names a different file than this seat has"));
                }
                let at = payload.get("at").and_then(Value::as_u64).unwrap_or(0);
                if at > crate::now_secs().saturating_add(UNPERSIST_SKEW) {
                    return Err(bad("`at` lies in the future"));
                }
                if at < meta.shared_ts {
                    return Err(bad("`at` lies before the share"));
                }
            }
        }
        Ok(())
    }
}
