// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs, dead_code)]

//! Shared helpers of the engine integration tests.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{
    ChatMessage, Command, EventEnvelope, GroupConfig, MemberId, MeshLink, Reply, SessionSettings, SessionView,
    Surface, WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::{EngineSink, NetError};

/// A deterministic non-nil message id for hand-built test envelopes (the
/// engine mints real random ids; this stands in for a peer's minting).
pub fn test_msg_id(seq: u64) -> molt_core::MessageId {
    let mut b = [0xa5u8; 16];
    b[..8].copy_from_slice(&seq.to_le_bytes());
    molt_core::MessageId(b)
}

/// One hand-built peer chat envelope carrying `test_msg_id(seq)` — what a
/// sender's outbox would hold for a plain text message. Stamped "now" so
/// the messages sit inside the chat-retention read window.
pub fn chat_env(seq: u64, from: &str, body: &str) -> EventEnvelope {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + seq;
    EventEnvelope { prev_seq: 0,
        seq,
        ts,
        by: from.to_string(),
        body: WorkspaceEvent::Chat(ChatMessage::text(test_msg_id(seq), from, body, ts)),
    }
}

/// Read a surface's applied log.
pub async fn read_applied(w: &WalletHandle, surface: Surface) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface,
            channel: None,
            view: None,
        })
        .await
        .expect("read state")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Read the chat surface's applied log.
pub async fn read_chat(w: &WalletHandle) -> Vec<serde_json::Value> {
    match w
        .execute(Command::ReadState {
            surface: Surface::Chat,
            channel: None,
            view: None,
        })
        .await
        .expect("read chat")
    {
        Reply::State(s) => s.applied,
        other => panic!("unexpected: {other:?}"),
    }
}

/// A `new file` git patch for `path` holding `content`.
pub fn wiki_add(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut patch = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for l in lines {
        patch.push('+');
        patch.push_str(l);
        patch.push('\n');
    }
    patch
}

/// How many documents the wiki base holds, as the engine serves it. The
/// snapshot carries the count since 2026-09-05 (§4.10); the tree itself
/// is the paged read's.
pub async fn wiki_docs(w: &WalletHandle) -> u64 {
    match w
        .execute(Command::ReadState {
            surface: Surface::Memory,
            channel: None,
            view: None,
        })
        .await
        .expect("read memory")
    {
        Reply::State(s) => s.wiki_docs,
        other => panic!("unexpected: {other:?}"),
    }
}

/// Poll the session until a running founding seals (`create.run.outcome
/// == 1`; panic on failure or after 20 s) — the sim ritual runs its
/// members asynchronously.
pub async fn await_founding(w: &WalletHandle) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    // ❻½: the founder's own phrase-backup confirmation gates the seal
    // (the sim members attest automatically) — played once, like a human
    let mut confirmed = false;
    loop {
        match w.execute(Command::ReadSession).await.expect("read session") {
            Reply::Session(s) => match s.create.run.outcome {
                1 => return,
                2 => panic!("founding failed: {:?}", s.create.run.log),
                _ => {
                    if !confirmed && !s.create.seed.is_empty() {
                        w.execute(Command::ConfirmSeedBackup {
                            phrase: s.create.seed.clone(),
                        })
                        .await
                        .expect("founder backup confirm");
                        confirmed = true;
                    }
                }
            },
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "founding did not seal in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Poll until the chat log holds at least `want` messages (or panic after
/// `secs`).
pub async fn await_chat_len(w: &WalletHandle, want: usize, secs: u64) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let chat = read_chat(w).await;
        if chat.len() >= want {
            return chat;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {want} chat messages, have {}",
            chat.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Read the full session view.
pub async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

/// An [`EngineSink`] that captures what a member-side supervisor delivers and
/// every passive-presence (`peer_seen`) sighting — the observation point for
/// mesh-resume and keepalive tests.
#[derive(Clone, Default)]
pub struct CaptureSink {
    delivered: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
    seen: Arc<Mutex<Vec<MemberId>>>,
}
impl CaptureSink {
    pub fn messages(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.delivered.lock().expect("lock").clone()
    }
    /// Every member `peer_seen` fired for, in order (duplicates kept).
    pub fn seen(&self) -> Vec<MemberId> {
        self.seen.lock().expect("lock").clone()
    }
}
impl EngineSink for CaptureSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.delivered.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, m: &MemberId) {
        self.seen.lock().expect("lock").push(m.clone());
    }
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

/// Run a real 2-of-2 founding + mesh bootstrap over the loopback hub: founder
/// engine on `root_a`, genuine member via `run_ritual_member`. Returns the
/// founder handle, the shared hub transport, the member's assembled mesh +
/// post-bootstrap MLS snapshot, and the workspace id. The founder is left one
/// `CreateFinish` short of entered — callers that need the workspace open must
/// execute it.
pub async fn found_with_mesh(
    root_a: &Path,
) -> (
    WalletHandle,
    molt_net::LoopbackTransport,
    Vec<MeshLink>,
    Vec<u8>,
    String,
) {
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: root_a.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) =
        molt_engine::__spawn_manual_founding_bootstrap(molt_core::GroupConfig::demo(), session_a);
    a.execute(Command::CreateStart {
        name: "Phoenix".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let materials = tokio::task::spawn_blocking(move || {
        material_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A hands out the invite material")
    })
    .await
    .expect("join blocking");
    let seat = materials.into_iter().next().expect("seat material");
    let hub = seat.transport.clone();

    let b_phrase = molt_storage::generate_seed_phrase().expect("b phrase");
    let b_task = tokio::spawn(async move {
        molt_engine::run_ritual_member(seat, "member-b".to_string(), b_phrase, true, true, None, None)
            .await
            .expect("B completes the member side + bootstrap")
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_session(&a).await.create.can_propose {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "member-b never joined");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.execute(Command::CreatePropose {
        name: "Phoenix".to_string(),
        agenda: "keep the mesh alive".to_string(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("founder proposes the charter");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate; the
    // None-ratifier member attests automatically)
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual must not fail: {:?}", s.create.run.log);
        if s.create.run.outcome == 1
            && s.create.run.log.iter().any(|l| l.contains("direct mesh established"))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the founder never bootstrapped its mesh; log: {:?}",
            s.create.run.log
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let b_outcome = b_task.await.expect("B task");
    let member_mesh = b_outcome.mesh.expect("B assembled its direct mesh");
    let member_mls = b_outcome.mls_snapshot.expect("member post-bootstrap snapshot");
    let id = read_session(&a).await.active_workspace.clone();
    assert!(!id.is_empty(), "the founded workspace is active");
    (a, hub, member_mesh, member_mls, id)
}

/// Holds every REQ for one of `kinds` for 12 s once armed: an onion relay
/// over Tor replaying a busy channel (field 2026-09-12). 12 s is past the
/// 10 s the ritual gates once allowed.
#[derive(Debug, Clone)]
pub struct SlowReplay {
    armed: Arc<std::sync::atomic::AtomicBool>,
    kinds: &'static [u16],
}

impl SlowReplay {
    pub fn new(kinds: &'static [u16]) -> Self {
        Self {
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kinds,
        }
    }

    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl nostr_relay_builder::builder::QueryPolicy for SlowReplay {
    fn admit_query<'a>(
        &'a self,
        query: &'a nostr_relay_builder::prelude::Filter,
        _addr: &'a std::net::SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult>
    {
        Box::pin(async move {
            let slow = query
                .kinds
                .as_ref()
                .is_some_and(|ks| ks.iter().any(|k| self.kinds.contains(&k.as_u16())));
            if slow && self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(12)).await;
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

/// A relay that stores events (a late subscriber is served the history),
/// behind `slow`. Keep the relay alive for the test's lifetime.
pub async fn slow_relay(slow: &SlowReplay) -> (nostr_relay_builder::LocalRelay, String) {
    use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
    let db = Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
        events: true,
        max_events: None,
    }));
    let relay = nostr_relay_builder::LocalRelay::new(
        nostr_relay_builder::builder::RelayBuilder::default()
            .database(db)
            .query_policy(slow.clone()),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// Once armed, the first kind-445 REQ stores `count` unreadable frames under
/// that REQ's `h` tags, stamped over the last five minutes: the resend flood
/// of a republic's locked-out nodes (findings R11/R12).
#[derive(Debug, Clone)]
pub struct Backlog {
    armed: Arc<std::sync::atomic::AtomicBool>,
    count: usize,
    /// Well-formed frames under a key nobody holds (counted unreadable),
    /// instead of malformed ones (dropped on sight).
    sealed: bool,
    db: Arc<nostr_relay_builder::prelude::MemoryDatabase>,
}

impl Backlog {
    pub fn new(count: usize) -> Self {
        use nostr_relay_builder::prelude::{MemoryDatabase, MemoryDatabaseOptions};
        Self {
            armed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            count,
            sealed: false,
            db: Arc::new(MemoryDatabase::with_opts(MemoryDatabaseOptions {
                events: true,
                max_events: None,
            })),
        }
    }

    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn sealed(count: usize) -> Self {
        Self { sealed: true, ..Self::new(count) }
    }

    pub fn db(&self) -> Arc<nostr_relay_builder::prelude::MemoryDatabase> {
        self.db.clone()
    }

    async fn inject(&self, tags: Vec<String>) {
        use nostr_relay_builder::prelude::{EventBuilder, Keys, Kind, NostrDatabase, Tag, Timestamp};
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        for i in 0..self.count {
            let h = &tags[i % tags.len()];
            let content = if self.sealed {
                molt_net::envelope::seal_outer(&[9u8; 32], format!("frame {i}").as_bytes()).expect("seal")
            } else {
                format!("bm90IGEgZnJhbWUg{i:08}")
            };
            let event = EventBuilder::new(Kind::Custom(445), content)
                .tag(Tag::parse(["h", h.as_str()]).expect("h tag"))
                .custom_created_at(Timestamp::from_secs(now - u64::try_from(i % 300).expect("small")))
                .sign_with_keys(&keys)
                .expect("sign");
            self.db.save_event(&event).await.expect("store");
        }
    }
}

impl nostr_relay_builder::builder::QueryPolicy for Backlog {
    fn admit_query<'a>(
        &'a self,
        query: &'a nostr_relay_builder::prelude::Filter,
        _addr: &'a std::net::SocketAddr,
    ) -> nostr_relay_builder::prelude::BoxedFuture<'a, nostr_relay_builder::builder::PolicyResult>
    {
        Box::pin(async move {
            use nostr_relay_builder::prelude::{Alphabet, SingleLetterTag};
            let group = query.kinds.as_ref().is_some_and(|ks| ks.iter().any(|k| k.as_u16() == 445));
            let tags: Vec<String> = query
                .generic_tags
                .get(&SingleLetterTag::lowercase(Alphabet::H))
                .map(|t| t.iter().cloned().collect())
                .unwrap_or_default();
            if group && !tags.is_empty() && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.inject(tags).await;
            }
            nostr_relay_builder::builder::PolicyResult::Accept
        })
    }
}

/// A storing relay that replays everything it holds per REQ (MockRelay
/// stops at 500), with `backlog` behind it.
pub async fn backlog_relay(backlog: &Backlog) -> (nostr_relay_builder::LocalRelay, String) {
    let relay = nostr_relay_builder::LocalRelay::new(
        nostr_relay_builder::builder::RelayBuilder::default()
            .database(backlog.db.clone())
            .default_filter_limit(100_000)
            .query_policy(backlog.clone()),
    );
    relay.run().await.expect("relay runs");
    let url = relay.url().await.to_string();
    (relay, url)
}

/// Hard-kill the engine (drop, never `CloseWorkspace`) and wait until its
/// writer thread released the workspace LOCK. Returns the workspace dir.
pub async fn hard_kill(a: WalletHandle, root: &Path, id: &str) -> std::path::PathBuf {
    drop(a);
    let dir = molt_storage::find_workspace_dir(root, id).expect("workspace dir");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match molt_storage::open_workspace(&dir) {
            Ok(_) => break, // lock free again; the guard drops right here
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the hard-killed engine never released the workspace lock"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    dir
}

/// A storage-backed engine over `root`, listing the workspaces already there.
pub fn engine(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: molt_storage::scan_workspaces(root)
            .iter()
            .map(molt_storage::ScanEntry::info)
            .collect(),
        settings: SessionSettings {
            workspace_dir: root.display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

/// A single-member group: threshold 1, no self-cosign - one `Approve`
/// applies, and no peer ever writes behind the test's back. It has NO chain
/// (the legacy counted path).
pub fn spawn_solo() -> WalletHandle {
    molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    )
}

/// Poll the session until `pred` holds, at most `secs` — the same
/// no-event-bus pattern every engine test uses.
pub async fn wait_for_secs(
    w: &WalletHandle,
    what: &str,
    secs: u64,
    pred: impl Fn(&SessionView) -> bool,
) -> Box<SessionView> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let s = read_session(w).await;
        if pred(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}\nsession: notice={:?} create.log={:?} join.log={:?} recover.log={:?}",
            s.notice,
            s.create.run.log,
            s.join.run.log,
            s.recover.log
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// [`wait_for_secs`] with the usual 30 s.
pub async fn wait_for(w: &WalletHandle, what: &str, pred: impl Fn(&SessionView) -> bool) -> Box<SessionView> {
    wait_for_secs(w, what, 30, pred).await
}

/// ADR-0004: nothing is pre-configured — each node adds and confirms the
/// relay itself. A `ws://127.0.0.1` relay is `Local` kind, gated like
/// clearnet (§10.14): confirm with the acknowledgement, then unlock the
/// session.
pub async fn adopt_relay(w: &WalletHandle, url: &str) {
    w.execute(Command::RelayAdd { url: url.to_string() })
        .await
        .expect("relay add");
    w.execute(Command::RelayConfirm {
        url: url.to_string(),
        accept_clearnet: true,
    })
    .await
    .expect("relay confirm");
    // B4: the confirmation lands on the PROBE's verdict, off-actor — an
    // unusable relay never becomes a confirmed one
    wait_for(w, "the relay probe to confirm the relay", |s| {
        s.settings
            .relays
            .iter()
            .any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    w.execute(Command::RelayClearnetSession { unlock: true })
        .await
        .expect("session unlock");
}

/// The second voice: wait until `w` sees the open proposal with this `op`,
/// then approve it through the public command surface.
pub async fn approve_op(w: &WalletHandle, op: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Reply::Proposals { proposals, .. } =
            w.execute(Command::LIST_ALL_PROPOSALS).await.expect("list proposals")
        {
            if let Some(p) = proposals.iter().find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(|v| v.as_str()) == Some(op)
            }) {
                w.execute(Command::Approve { proposal: p.id, note: None })
                    .await
                    .expect("approve");
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the {op} proposal never reached the second voice"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// E4: the graph reads answer `index_building: true` with empty lists
/// instead of refusing, so a test waits that out too.
pub fn still_building(reply: &Reply) -> bool {
    matches!(
        reply,
        Reply::WikiHealth { index_building: true, .. }
            | Reply::WikiNeighbors { index_building: true, .. }
            | Reply::WikiSearch { index_building: true, .. }
    )
}

/// Found a real 2-of-2 republic over the relay; both engines end up entered.
pub async fn found_pair(
    root: &std::path::Path,
    urls: &[&str],
    name: &str,
) -> (WalletHandle, WalletHandle) {
    let a = engine(&root.join("founder"));
    for url in urls {
        adopt_relay(&a, url).await;
    }
    a.execute(Command::CreateStart {
        name: name.to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let s = wait_for(&a, "the seat link to become joinable", |s| {
        !s.create.seats.is_empty()
            && molt_engine::FoundingInvite::parse(&s.create.seats[0].link).is_ok()
    })
    .await;
    let link = s.create.seats[0].link.clone();

    let b = engine(&root.join("member"));
    for url in urls {
        adopt_relay(&b, url).await;
    }
    b.execute(Command::JoinStart {
        invite: link,
        member: "walter".to_string(),
        relays: Vec::new(),
    })
    .await
    .expect("join starts");
    wait_for(&a, "the founder to accept the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: name.to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    // ❻½: the founder's phrase-backup confirmation (n-of-n gate)
    {
        let seed_ = read_session(&a).await.create.seed.clone();
        a.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("founder backup confirm");
    }
    wait_for(&b, "walter to see the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    {
        let seed_ = read_session(&b).await.join.seed.clone();
        b.execute(Command::ConfirmSeedBackup { phrase: seed_ })
            .await
            .expect("joiner backup confirm");
    }
    wait_for(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    // entering is gated on the phrase-backup step now (2026-08-08) — both ends
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_for(&b, "the join to seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty())
        .await;
    // entering is gated on the phrase-backup step now (2026-08-08)
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_for(&b, "the joiner to enter", |s| {
        s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()
    })
    .await;
    (a, b)
}

/// What a member signs for `change` at `height` on `w`'s head: the
/// `molt-chain-change-v3` bytes every founding of this build ratifies.
#[allow(dead_code)]
pub async fn signed_on_head(w: &WalletHandle, rid: &str, height: u64, change: &molt_core::ChainChange) -> Vec<u8> {
    let Ok(molt_core::Reply::Status(s)) = w.execute(Command::Status).await else {
        panic!("status answers Reply::Status");
    };
    molt_core::chain::approval_bytes_v3(rid, height, &s.chain_sync.head_hash, change)
}

/// `askers` hold `sender`'s stream behind a lost `t0` while `span` chats
/// arrive (with `propose`, a proposal after them), asking nothing and
/// settling nothing; then each closes and reopens: what it held is gone and
/// the relay copies of it no longer open. Returns the reopen instant.
#[allow(dead_code)]
pub async fn hold_tail_and_reopen(
    sender: &WalletHandle,
    askers: &[&WalletHandle],
    span: usize,
    propose: bool,
) -> std::time::Instant {
    for b in askers {
        // the probe is off in v0.1.3; its tests keep it running
        b.__force_open_probe(true);
        b.__hold_gap_wants(true);
        b.__drop_inbound_chat("t0");
    }
    for i in 0..span {
        sender
            .execute(Command::Chat {
                body: format!("t{i}"),
                quote: None,
                channel: molt_core::ChannelRef::Group,
            })
            .await
            .expect("chat");
    }
    if propose {
        sender
            .execute(Command::Propose {
                surface: molt_core::Surface::Organization,
                payload: serde_json::json!({"op": "set_name", "value": "Far"}),
            })
            .await
            .expect("propose");
    }
    // a seam holds the gap beat still, so this wait is not a race
    for b in askers {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        loop {
            let held = match b.execute(Command::Status).await.expect("status") {
                molt_core::Reply::Status(s) => s.inbound_held.iter().map(|h| h.held).max().unwrap_or(0),
                other => panic!("unexpected: {other:?}"),
            };
            if usize::try_from(held).unwrap_or(usize::MAX) + 1 >= span {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the span was never held: {held}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let mut ids = Vec::new();
    for b in askers {
        ids.push(read_session(b).await.active_workspace.clone());
        b.execute(Command::CloseWorkspace).await.expect("close");
        b.__hold_gap_wants(false);
    }
    let reopened = std::time::Instant::now();
    for (b, id) in askers.iter().zip(ids) {
        b.execute(Command::OpenWorkspace { id }).await.expect("reopen");
    }
    reopened
}

/// Wait until `w` shows the chats `t0..t{span}`: it must say it is behind
/// on `sender` within `grace`, and on every look from then on while any is
/// missing. The elapsed time since `from`.
#[allow(dead_code)]
pub async fn fetches_the_tail_and_says_so(
    w: &WalletHandle,
    sender: &str,
    span: usize,
    from: std::time::Instant,
    grace: Duration,
    within: Duration,
) -> Duration {
    let want: Vec<String> = (0..span).map(|i| format!("t{i}")).collect();
    let mut shown = false;
    loop {
        // the status first: a tail completing between the two reads must
        // not read as "short and not behind"
        let behind = match w.execute(Command::Status).await.expect("status") {
            molt_core::Reply::Status(s) => s.behind.iter().any(|m| m == sender),
            other => panic!("unexpected: {other:?}"),
        };
        let have: Vec<String> = read_chat(w)
            .await
            .iter()
            .filter_map(|r| r.get("body").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        let missing = want.iter().filter(|b| !have.contains(b)).count();
        if missing == 0 {
            return from.elapsed();
        }
        assert!(behind || !shown, "{missing} chats short, and behind was cleared");
        shown |= behind;
        assert!(shown || from.elapsed() < grace, "{missing} chats short, never shown behind");
        assert!(from.elapsed() < within, "{missing} chats never arrived");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
