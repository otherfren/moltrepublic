// SPDX-License-Identifier: GPL-3.0-or-later

//! The gated surfaces: propose → threshold approvals → apply. A faithful
//! but *simulated* stand-in for the real FROST threshold machine.

use std::collections::HashMap;

use molt_core::{
    ChannelInfo, ChannelRef, Event, MemberView, MemberVote, MoltError, ProposalId,
    ProposalRecord, ProposalState, ProposalView, Reply, StatusView, Surface, SurfaceSnapshot,
    SurfaceStat, UploadView, VoteState, WorkspaceEvent,
};
use serde_json::Value;

use crate::State;

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
}

/// Parse a retention window ("14 days" or a bare "14") into days.
/// `None` when unparseable or outside 1..=365 — callers refuse, never guess.
pub(crate) fn parse_retention_days(value: &str) -> Option<u64> {
    let days: u64 = value.split_whitespace().next()?.parse().ok()?;
    (1..=365).contains(&days).then_some(days)
}

/// The hard cap on a republic image (decoded bytes). The bytes ride the
/// proposal payload — sign-what-you-see: every member votes on the actual
/// image — so the payload must stay a small gossip frame, not a file drop.
pub(crate) const ORG_IMAGE_MAX_BYTES: usize = 256 * 1024;

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
    let refuse = || {
        MoltError::BadPayload(
            "the image cannot be decoded (png/jpeg/webp/gif/bmp/svg)".into(),
        )
    };
    // an SVG travels as its source text: prefix sniff
    let head = std::str::from_utf8(&bytes[..bytes.len().min(1024)]).unwrap_or("");
    let trimmed = head.trim_start();
    if trimmed.starts_with("<svg") || trimmed.starts_with("<?xml") {
        return Ok(());
    }
    image::guess_format(bytes).map_err(|_| refuse())?;
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| refuse())?
        .into_dimensions()
        .map_err(|_| refuse())?;
    if w == 0 || h == 0 || w > IMAGE_MAX_DIM || h > IMAGE_MAX_DIM {
        return Err(MoltError::BadPayload(format!(
            "the image is {w}x{h} — the limit is {IMAGE_MAX_DIM}x{IMAGE_MAX_DIM}"
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
        "set_chat_retention" => format!("{} days", eff.retention_days),
        // the image ops show what they change: the current image reference
        "set_image" | "remove_image" => eff.image.clone(),
        // no plugin state exists yet (mock) — nothing to show
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
        // members vote on the image, not on a path only the proposer has)
        "set_image" => match image_bytes(payload) {
            None => Err(MoltError::BadPayload(
                "a set_image proposal must embed the image (base64 `bytes_b64`)".into(),
            )),
            Some(bytes) if bytes.len() > ORG_IMAGE_MAX_BYTES => Err(MoltError::BadPayload(
                format!(
                    "the image is too large ({} KiB) — the limit is {} KiB",
                    bytes.len() / 1024,
                    ORG_IMAGE_MAX_BYTES / 1024
                ),
            )),
            // WP3: within the cap, the bytes must also decode as a picture
            Some(bytes) => image_decodable(&bytes),
        },
        _ => Ok(()),
    }
}

impl State {
    pub(crate) fn cmd_propose(
        &mut self,
        surface: Surface,
        payload: Value,
    ) -> Result<Reply, MoltError> {
        if !surface.is_gated() {
            return Err(MoltError::ChatNotGated);
        }
        if !payload.is_object() {
            return Err(MoltError::BadPayload(
                "payload must be a JSON object".into(),
            ));
        }
        validate_org_payload(surface, &payload)?;
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
        self.emit(Event::Proposed { id, surface });
        if self.is_chain_governed() {
            // real threshold: the proposer co-signs their own proposal; the
            // other members' signatures arrive over the mesh
            if self.config.self_cosign {
                self.chain_sign_and_gossip_approval(id.0);
            }
        } else {
            if self.config.self_cosign {
                // legacy counted simulation — the proposer's own approval is an
                // event too, so replay must not depend on the config flag
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
            // A self-cosign may already satisfy a threshold of 1.
            self.try_apply(id);
        }
        Ok(Reply::Proposed { id })
    }

    pub(crate) fn cmd_approve(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
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

    pub(crate) fn cmd_decline(&mut self, proposal: ProposalId) -> Result<Reply, MoltError> {
        {
            let p = self
                .proposals
                .get(&proposal.0)
                .ok_or(MoltError::UnknownProposal(proposal))?;
            if p.state != ProposalState::Proposed {
                return Err(MoltError::AlreadyTerminal(proposal, p.state));
            }
        }
        let me = self.member();
        let env = self.make_env(
            me.clone(),
            WorkspaceEvent::Declined {
                id: proposal,
                by: me,
            },
        );
        self.record(env);
        self.emit(Event::Rejected { id: proposal });
        Ok(Reply::Ack)
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
    }

    /// Re-decide thresholds after a replay: a crash between an `Approved`
    /// frame and its `Applied` frame must not leave a proposal stuck at
    /// `have >= need` forever. Called once per open, after the tail applied.
    ///
    /// Legacy path only: a chain-governed workspace never applies by counting —
    /// the replayed `Approved` frames are real signatures the chain already
    /// consumed (or not), so re-counting them here would double-apply.
    pub(crate) fn recover_pending_applies(&mut self) {
        if self.is_chain_governed() {
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
        // collected signatures, not the legacy counter
        let approvals = if self.is_chain_governed() {
            self.chain_approval_count(id)
        } else {
            p.approvals
        };
        // reader-relative: chain governance knows exactly who signed; the
        // legacy counted simulation has one local operator standing in for
        // the whole group, where the FIRST approval is by definition ours
        // (self-cosign or the explicit approve) and repeats simulate peers
        let approved_by_me = if self.is_chain_governed() {
            let me = self.member();
            self.pending_sigs
                .get(&id)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == me))
        } else {
            p.approvals > 0
        };
        let (current, proposed) = change_summary(&self.org_effective(), p);
        // the voting row: one stance per roster member, roster order. Chain
        // governance knows exactly who signed; the legacy counted simulation
        // attributes its anonymous counter deterministically (the local
        // member first — matching `approved_by_me` — then roster order), so
        // the row always agrees with the `approvals` count.
        let me = self.member();
        let mut votes: Vec<MemberVote> = if self.is_chain_governed() {
            let signed: Vec<String> = self
                .pending_sigs
                .get(&id)
                .map(|s| s.sigs.iter().map(|a| a.member.clone()).collect())
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
            let mut others_left = approvals.saturating_sub(1);
            self.roster()
                .into_iter()
                .map(|member| {
                    let vote = if member == me {
                        if approvals > 0 {
                            VoteState::Approved
                        } else {
                            VoteState::Open
                        }
                    } else if others_left > 0 {
                        others_left -= 1;
                        VoteState::Approved
                    } else {
                        VoteState::Open
                    };
                    MemberVote { member, vote }
                })
                .collect()
        };
        // a rejected proposal names its decliner: that roster row shows the
        // veto instead of an open/approved stance
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
            current,
            proposed,
            votes,
            declined_at: p.declined_at,
            declined_by: p.declined_by.clone(),
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
                _ => {}
            }
        }
        eff
    }

    /// The applied Organization entries, BORROWED (legacy counted projection
    /// then the chain projection — one is always empty for a workspace). No
    /// clone: callers only read `op`/`value`/`bytes_b64` fields.
    pub(crate) fn applied_org_entries(&self) -> impl Iterator<Item = &Value> {
        self.applied
            .get(&Surface::Organization)
            .into_iter()
            .flatten()
            .chain(self.chain_applied.get(&Surface::Organization).into_iter().flatten())
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
/// it, like the `*_label_at` helpers): does the given chat sub-view admit
/// a message of that age?
///
/// The window splits at its half: `"today"` (the General view) admits the
/// younger half (age ≤ 50 % of the window, boundary inclusive), `"archive"`
/// the older half still inside the window (50 % < age ≤ 100 %), `None` the
/// whole window — today's unfiltered read, so older readers keep their
/// behavior. Anything past 100 % stays hidden everywhere ("deleted"). A
/// timestamp of 0 (legacy/unknown age) must never silently vanish: it
/// files under the general view and the unfiltered read, never the archive.
/// The whole-window arm delegates to [`State::aged_out_at`] — the same
/// predicate the share-expiry point checks use — so the two can't drift.
pub(crate) fn chat_view_admits(
    view: Option<&str>,
    ts: u64,
    now: u64,
    retention_days: u64,
) -> bool {
    let window = retention_days * 86_400;
    let cutoff = now.saturating_sub(window);
    let half = now.saturating_sub(window / 2);
    match (view, ts) {
        (Some("archive"), 0) => false,
        (_, 0) => true,
        (Some("archive"), ts) => ts >= cutoff && ts < half,
        // any other validated key is the general ("today") view
        (Some(_), ts) => ts >= half,
        (None, ts) => !State::aged_out_at(cutoff, ts),
    }
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

    /// [`State::chat_visible`] narrowed to one retention sub-view
    /// ("today"/"archive", `None` = the whole window) — the read contract
    /// behind `ReadState { view }`, shared by GUI and MCP (co-equality).
    pub(crate) fn chat_visible_in<'a>(
        &'a self,
        view: Option<&'a str>,
    ) -> impl Iterator<Item = &'a molt_core::ChatMessage> + 'a {
        let now = crate::now_secs();
        let days = self.org_effective().retention_days;
        self.chat
            .iter()
            .filter(move |m| chat_view_admits(view, m.ts, now, days))
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
            // the surface's applied log is the legacy (counted-simulation)
            // projection plus the chain (real threshold) projection — one of the
            // two is always empty for a given workspace, so this is a concat
            let mut v = self.applied.get(&surface).cloned().unwrap_or_default();
            if let Some(chain) = self.chain_applied.get(&surface) {
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
        }];
        let mut pos: HashMap<ChannelRef, usize> = HashMap::from([(ChannelRef::Group, 0)]);
        for m in self.chat_visible() {
            let at = *pos.entry(m.channel.clone()).or_insert_with(|| {
                infos.push(ChannelInfo {
                    channel: m.channel.clone(),
                    count: 0,
                    last_ts: 0,
                    state: None,
                });
                infos.len() - 1
            });
            infos[at].count += 1;
            infos[at].last_ts = infos[at].last_ts.max(m.ts);
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
        let (applied_ids, applied) = self
            .applied_values(surface, channel.as_ref(), view)
            .into_iter()
            .unzip();
        SurfaceSnapshot {
            surface,
            gated: surface.is_gated(),
            applied,
            applied_ids,
            pending,
            denied: declined.len(),
            declined,
            channels: if surface == Surface::Chat {
                self.chat_channels()
            } else {
                Vec::new()
            },
        }
    }

    /// Whether a pending proposal still awaits `member`'s approval. Chain
    /// governance knows exactly who signed; the legacy counted simulation
    /// (one operator stands in for the group) treats the first approval as
    /// the local member's and cannot know about the simulated peers — for
    /// them every pending proposal counts as open (mock).
    fn waits_on(&self, id: u64, p: &ProposalRecord, member: &str) -> bool {
        if p.state != ProposalState::Proposed {
            return false;
        }
        if self.is_chain_governed() {
            !self
                .pending_sigs
                .get(&id)
                .is_some_and(|s| s.sigs.iter().any(|a| a.member == member))
        } else if member == self.member() {
            p.approvals == 0
        } else {
            true
        }
    }

    /// The Organization → Members table: one row per roster member. The
    /// identity anchor comes from the genesis (real on ritual-founded
    /// workspaces); presence is the session entry's mock label.
    pub(crate) fn members_view(&self) -> Vec<MemberView> {
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
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
                let (last_seen, presence) = entry
                    .and_then(|e| e.members.iter().find(|m| m.name == member))
                    .map(|m| (m.last.clone(), m.state))
                    .unwrap_or_default();
                MemberView {
                    open_proposals: self
                        .proposals
                        .iter()
                        .filter(|(pid, p)| self.waits_on(**pid, p, &member))
                        .count(),
                    uploads: self
                        .chat_visible()
                        .filter(|m| m.from == member && m.file.is_some())
                        .count(),
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
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let presence = |member: &str| {
            entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.state)
                .unwrap_or(0)
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
                    expires_ts: if m.ts == 0 { 0 } else { m.ts + retention_secs },
                    online: m.from == me || presence(&m.from) != 2,
                    // the sharer's log-anchored sha256 ("" = legacy share,
                    // honestly unknown) — what a download must reproduce
                    checksum: f.checksum.clone(),
                    download: self.downloads.get(&m.id).cloned(),
                })
            })
            .collect()
    }

    pub(crate) fn status(&self) -> StatusView {
        // the activity trio is a mock presence projection (real presence is
        // transport work): synced = hour-active, syncing = day-active, the
        // whole roster = week-active
        let entry = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace);
        let presence = |member: &str| {
            entry
                .and_then(|e| e.members.iter().find(|mi| mi.name == member))
                .map(|mi| mi.state)
                .unwrap_or(0)
        };
        let roster = self.roster();
        let active_1h = roster.iter().filter(|m| presence(m) == 0).count();
        let active_24h = roster.iter().filter(|m| presence(m) <= 1).count();
        let active_7d = roster.len();
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
                        + self.chain_applied.get(&s).map(|v| v.len()).unwrap_or(0)
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
            // recovery exists exactly here — the frontends key the per-member
            // "recovery link" action on this (never on the member's presence:
            // a recovery link is FOR an unreachable member)
            chain_governed: self.is_chain_governed(),
        }
    }
}
