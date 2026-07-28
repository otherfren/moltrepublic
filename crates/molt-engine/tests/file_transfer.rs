// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Peer-to-peer file transfer END TO END across the founding mesh: a real
//! founder engine on one side, a genuinely separate member runtime on the
//! other (raw supervisor + MLS group, the `founding_chats_over_the_direct_mesh`
//! scaffold). Both directions are proven:
//!
//! * member shares → the FOUNDER ENGINE downloads (`Command::DownloadFile`
//!   drives the whole engine path: MLS-encrypted `FileRequested` over the
//!   mesh, the transfer streams to disk, `read_uploads` reports the phases);
//! * founder shares via `Command::ShareFile` (real hash, real checksum) →
//!   the member fetches with a HAND-ROLLED requester (a second, independent
//!   implementation of the wire format), exercising the engine's serve arm
//!   (`FileRequested` → authenticate → stream `Manifest` + `Piece`s).
//!
//! The bytes never enter the event log — only the MLS-encrypted request
//! does — and every transfer verifies against the log-anchored sha256.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use molt_core::{
    ChatMessage, Command, EventEnvelope, FileMeta, MemberId, MoltError, Reply, SessionSettings,
    SessionView, WorkspaceEvent,
};
use molt_engine::WalletHandle;
use molt_net::supervisor::{self, MemLog, MemStateStore, NetConfig};
use molt_net::transfer::{
    decode_frame, encode_ack, pieces_for, FetchRequest, TransferAck, TransferFrame,
};
use molt_net::{
    invite, msg_id, EngineSink, MlsChannel, MlsIncoming, MlsMember, NetError, PeerLink, QueueId,
    Reassembler, SndQueueAddr, Transport, WrapKey,
};
use sha2::Digest as _;
use tokio::sync::watch;

async fn read_session(w: &WalletHandle) -> Box<SessionView> {
    match w.execute(Command::ReadSession).await.expect("read session") {
        Reply::Session(s) => s,
        other => panic!("unexpected: {other:?}"),
    }
}

async fn read_uploads(w: &WalletHandle) -> Vec<molt_core::UploadView> {
    match w.execute(Command::ReadUploads).await.expect("read uploads") {
        Reply::Uploads { uploads: rows } => rows,
        other => panic!("unexpected: {other:?}"),
    }
}

/// A test-only sink recording what the member supervisor delivers.
#[derive(Clone, Default)]
struct RecordSink {
    got: Arc<Mutex<Vec<(MemberId, EventEnvelope)>>>,
}
impl RecordSink {
    fn messages(&self) -> Vec<(MemberId, EventEnvelope)> {
        self.got.lock().expect("lock").clone()
    }
}
impl EngineSink for RecordSink {
    async fn deliver(&self, from: &MemberId, env: EventEnvelope) -> Result<(), NetError> {
        self.got.lock().expect("lock").push((from.clone(), env));
        Ok(())
    }
    async fn peer_seen(&self, _m: &MemberId) {}
    async fn send_failed(&self, _m: &MemberId, _r: &str) {}
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Deterministic pseudo-random content.
fn content(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from((i * 37 + 11) % 249).expect("byte range"))
        .collect()
}

/// Parse a reply handover into its send address + wrap key.
fn parse_handover(h: &invite::ReplyHandover) -> (SndQueueAddr, WrapKey) {
    let qid = hex::decode(&h.queue_id).expect("handover queue id");
    let wrap: [u8; 32] = hex::decode(&h.wrap)
        .expect("handover wrap")
        .try_into()
        .expect("32-byte wrap key");
    (
        SndQueueAddr {
            server: h.server.clone(),
            id: QueueId::from_bytes(qid),
        },
        WrapKey::from_bytes(wrap),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_file_downloads_peer_to_peer_across_the_mesh() {
    let tmp = tempfile::tempdir().expect("tmp");
    let session_a = SessionView {
        workspaces: Vec::new(),
        settings: SessionSettings {
            workspace_dir: tmp.path().join("founder").display().to_string(),
            ..SessionSettings::default()
        },
        ..SessionView::default()
    };
    let (a, material_rx) = molt_engine::__spawn_manual_founding_bootstrap(
        molt_core::GroupConfig::demo(),
        session_a,
    );
    a.execute(Command::CreateStart {
        name: "Transfer Club".to_string(),
        member: "founder-a".to_string(),
        threshold: 2,
        members: 2,
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
        name: "Transfer Club".to_string(),
        agenda: "move real bytes".to_string(),
    })
    .await
    .expect("propose charter");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = read_session(&a).await;
        assert_ne!(s.create.run.outcome, 2, "ritual failed: {:?}", s.create.run.log);
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
    a.execute(Command::CreateFinish).await.expect("enter");

    // --- the member's runtime: raw supervisor + the SHARED MLS group arc
    // (the test encrypts/decrypts with the same ratchet the channel uses) ---
    let links: Vec<PeerLink> = member_mesh.iter().filter_map(PeerLink::from_mesh).collect();
    assert_eq!(links.len(), 1, "one link, to the founder");
    let member_group = Arc::new(Mutex::new(
        MlsMember::restore(&member_mls).expect("restore member MLS"),
    ));
    let member_feed = MemLog::new();
    let member_sink = RecordSink::default();
    let (member_wake, member_wake_rx) = watch::channel(0u64);
    let _member_sup = supervisor::spawn(
        hub.clone(),
        NetConfig::fast("member-b".to_string(), links, 7),
        member_feed.clone(),
        MemStateStore::new(),
        member_sink.clone(),
        member_wake_rx,
        Some(MlsChannel::from_shared(member_group.clone())),
    );

    // =====================================================================
    // Direction 1: the MEMBER shares — the FOUNDER ENGINE downloads.
    // =====================================================================
    let member_bytes = content(300 * 1024 + 5); // 2 pieces
    let member_dir = tmp.path().join("member-files");
    std::fs::create_dir_all(&member_dir).expect("member dir");
    let member_file = member_dir.join("protokoll.pdf");
    std::fs::write(&member_file, &member_bytes).expect("member file");
    let member_sha = hex::encode(sha2::Sha256::digest(&member_bytes));
    let share_id = common::test_msg_id(42);
    let ts = now();
    let mut share_msg = ChatMessage::text(share_id, "member-b", "", ts);
    share_msg.file = Some(FileMeta {
        name: "protokoll.pdf".to_string(),
        size: u64::try_from(member_bytes.len()).expect("len fits u64"),
        kind: "PDF".to_string(),
        modified: ts,
        available: true,
        checksum: member_sha.clone(),
    });
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 1,
        ts,
        by: "member-b".to_string(),
        body: WorkspaceEvent::Chat(share_msg),
    });
    let _ = member_wake.send(1);
    // the founder sees the member's share
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_uploads(&a).await.iter().any(|u| u.name == "protokoll.pdf") {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the share never reached the founder");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // the founder kicks the download off (the full engine path)
    let dl_dir = tmp.path().join("founder-downloads");
    std::fs::create_dir_all(&dl_dir).expect("dl dir");
    a.execute(Command::DownloadFile {
        id: share_id,
        dest: Some(dl_dir.display().to_string()),
    })
    .await
    .expect("download kickoff");

    // the member receives the MLS-encrypted fetch request over the mesh …
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let req: FetchRequest = loop {
        let hit = member_sink.messages().into_iter().find_map(|(_, env)| {
            if let WorkspaceEvent::FileRequested { ct } = env.body {
                Some(ct)
            } else {
                None
            }
        });
        if let Some(ct) = hit {
            let raw = hex::decode(&ct).expect("ct hex");
            let (from, plain) = match member_group
                .lock()
                .expect("lock")
                .decrypt(&raw)
                .expect("group decrypt")
            {
                MlsIncoming::Application { from, plaintext } => (from, plaintext),
                other => panic!("unexpected MLS message: {other:?}"),
            };
            assert_eq!(from, "founder-a", "MLS authenticates the requester");
            break serde_json::from_slice(&plain).expect("fetch request json");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the fetch request never reached the member"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(req.id, share_id.to_string());
    assert!(req.expires > now(), "the request carries a future expiry");

    // … and serves it with a hand-rolled sender (Manifest + 2 Pieces —
    // within the ack window, so the serve needs no ack reads)
    {
        let (reply_snd, reply_wrap) = parse_handover(&req.reply);
        let ack_q = hub.create_queue().await.expect("ack queue");
        let manifest = TransferFrame::Manifest {
            id: req.id.clone(),
            size: u64::try_from(member_bytes.len()).expect("len fits u64"),
            pieces: pieces_for(u64::try_from(member_bytes.len()).expect("len fits u64")),
            sha256: member_sha.clone(),
            ack: invite::ReplyHandover {
                server: ack_q.snd.server.clone(),
                queue_id: hex::encode(&ack_q.snd.id.0),
                wrap: hex::encode(WrapKey::fresh().expect("wrap").to_bytes()),
            },
        };
        let bytes = molt_net::transfer::encode_frame(&manifest).expect("encode manifest");
        supervisor::send_framed(&hub, &reply_snd, &reply_wrap, msg_id(&req.id, "fetch", 0), &bytes)
            .await
            .expect("send manifest");
        for (i, piece) in member_bytes.chunks(molt_net::transfer::PIECE_LEN).enumerate() {
            let frame = TransferFrame::Piece {
                index: u32::try_from(i).expect("small index"),
                bytes: piece.to_vec(),
            };
            let bytes = molt_net::transfer::encode_frame(&frame).expect("encode piece");
            supervisor::send_framed(
                &hub,
                &reply_snd,
                &reply_wrap,
                msg_id(&req.id, "fetch", u64::try_from(i).expect("small index") + 1),
                &bytes,
            )
            .await
            .expect("send piece");
        }
    }

    // the founder's uploads view walks the phases to done-with-path, and
    // the file lands byte-identical
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let final_path = loop {
        let rows = read_uploads(&a).await;
        let row = rows
            .iter()
            .find(|u| u.id == share_id)
            .expect("the share row exists");
        if let Some(d) = &row.download {
            assert_ne!(d.phase, "failed", "download failed: {}", d.error);
            if d.phase == "done" {
                break d.path.clone();
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the download never completed: {:?}",
            rows.iter().find(|u| u.id == share_id).and_then(|u| u.download.clone())
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(final_path, dl_dir.join("protokoll.pdf").display().to_string());
    assert_eq!(
        std::fs::read(&final_path).expect("downloaded file"),
        member_bytes,
        "byte-identical across the mesh"
    );

    // negative: the member removes the file → the founder's next download
    // is refused synchronously (the share is unavailable for everyone)
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 2,
        ts: now(),
        by: "member-b".to_string(),
        body: WorkspaceEvent::FileRemoved {
            index: 0,
            id: Some(share_id),
            by: "member-b".to_string(),
        },
    });
    let _ = member_wake.send(2);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if read_uploads(&a).await.iter().any(|u| u.id == share_id && !u.available) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the removal never reached the founder"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(
        a.execute(Command::DownloadFile { id: share_id, dest: None }).await,
        Err(MoltError::FileUnavailable(i)) if i == share_id
    ));

    // =====================================================================
    // Direction 2: the FOUNDER ENGINE shares — the member fetches with a
    // hand-rolled requester (exercising the engine's SERVE arm).
    // =====================================================================
    let founder_bytes = content(90 * 1024); // 1 piece
    let founder_file = tmp.path().join("satzung.md");
    std::fs::write(&founder_file, &founder_bytes).expect("founder file");
    a.execute(Command::ShareFile {
        path: founder_file.display().to_string(),
        channel: molt_core::ChannelRef::default(),
    })
    .await
    .expect("founder shares");
    // wait for the engine's async share post; grab id + anchored checksum
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (a_share_id, a_share_sha) = loop {
        if let Some(row) = read_uploads(&a).await.into_iter().find(|u| u.name == "satzung.md") {
            break (row.id, row.checksum);
        }
        assert!(tokio::time::Instant::now() < deadline, "the founder's share never posted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        a_share_sha,
        hex::encode(sha2::Sha256::digest(&founder_bytes)),
        "the engine anchored the real sha256"
    );

    // the member's hand-rolled fetch: mint the reply queue, advertise it
    // MLS-encrypted through the mesh, reassemble the engine's frames
    let reply_q = hub.create_queue().await.expect("reply queue");
    let reply_wrap = WrapKey::fresh().expect("reply wrap");
    let mut rx = hub.subscribe(&reply_q.rcv).await.expect("subscribe");
    let req = FetchRequest {
        id: a_share_id.to_string(),
        reply: invite::ReplyHandover {
            server: reply_q.snd.server.clone(),
            queue_id: hex::encode(&reply_q.snd.id.0),
            wrap: hex::encode(reply_wrap.to_bytes()),
        },
        expires: now() + 600,
    };
    let ct = {
        let mut g = member_group.lock().expect("lock");
        hex::encode(g.encrypt(&serde_json::to_vec(&req).expect("json")).expect("encrypt"))
    };
    member_feed.push(EventEnvelope { prev_seq: 0,
        seq: 3,
        ts: now(),
        by: "member-b".to_string(),
        body: WorkspaceEvent::FileRequested { ct },
    });
    let _ = member_wake.send(3);

    // receive Manifest + Piece(s), ack each piece, verify the bytes
    let mut reasm = Reassembler::new();
    let mut got: Vec<u8> = Vec::new();
    let mut manifest: Option<(u64, u32, String, SndQueueAddr, WrapKey)> = None;
    let mut acked = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    'transfer: loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let delivery = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("the engine's serve answered in time")
            .expect("reply queue open");
        let Ok(plain) = molt_net::wrap::unwrap_block(&reply_wrap, &delivery.block) else {
            delivery.ack.ack();
            continue;
        };
        let outcome = reasm.push(&plain);
        delivery.ack.ack();
        let Ok(molt_net::chunk::PushOutcome::Complete(_, bytes)) = outcome else {
            continue;
        };
        match decode_frame(&bytes).expect("frame decodes") {
            TransferFrame::Manifest { id, size, pieces, sha256, ack } => {
                assert_eq!(id, a_share_id.to_string());
                assert_eq!(size, u64::try_from(founder_bytes.len()).expect("len fits u64"));
                assert_eq!(pieces, 1);
                assert_eq!(sha256, a_share_sha, "the served hash matches the anchor");
                manifest = Some((size, pieces, sha256, parse_handover(&ack).0, parse_handover(&ack).1));
            }
            TransferFrame::Piece { index, bytes } => {
                let (size, _, _, ack_snd, ack_wrap) =
                    manifest.as_ref().expect("manifest before pieces");
                got.extend_from_slice(&bytes);
                acked += 1;
                let ack_frame =
                    encode_ack(&TransferAck::Received { index }).expect("encode ack");
                supervisor::send_framed(
                    &hub,
                    ack_snd,
                    ack_wrap,
                    msg_id(&a_share_id.to_string(), "ack", acked),
                    &ack_frame,
                )
                .await
                .expect("send ack");
                if u64::try_from(got.len()).expect("len fits u64") >= *size {
                    break 'transfer;
                }
            }
            TransferFrame::Refused { reason, .. } => {
                panic!("the engine refused a valid request: {reason}");
            }
        }
    }
    assert_eq!(got, founder_bytes, "the member received the founder's bytes byte-identical");
    assert_eq!(
        hex::encode(sha2::Sha256::digest(&got)),
        a_share_sha,
        "the fetched bytes hash to the log-anchored checksum"
    );
}
