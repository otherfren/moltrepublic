// SPDX-License-Identifier: GPL-3.0-or-later

//! The single-operator governance path (propose / approve / decline /
//! withdraw, applied logs, the org tables and the proposal card views).

use super::support::*;
use crate::*;
use serde_json::json;

#[test]
fn propose_then_threshold_applies() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: the proposal genuinely waits for a
        // vote, and this node's OWN single approval honestly meets the
        // threshold — no peer is ever counted for.
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert_eq!(
            read_surface(&w, Surface::Memory).await.pending.len(),
            1,
            "no self-cosign: the proposal waits for this node's vote"
        );
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        match w
            .execute(Command::ReadState {
                surface: Surface::Memory,
                channel: None,
                view: None,
            })
            .await
            .expect("read")
        {
            Reply::State(s) => {
                assert_eq!(s.applied.len(), 1, "note should be applied at threshold");
                assert!(s.pending.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// Without chain governance this node records at most its OWN approval.
/// The pre-chain counting simulation (a repeated `Approve` counted as
/// the next member's co-signature) is gone from the production path: a
/// repeat is refused with an honest error, the counter never moves, and
/// no proposal applies on invented peer approvals.
#[test]
fn approve_never_counts_invented_peer_approvals() {
    rt().block_on(async {
        // self_cosign: proposing already recorded my one real approval
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        for _ in 0..2 {
            let err = w
                .execute(Command::Approve { proposal: id })
                .await
                .expect_err("a second local approval cannot stand in for a peer");
            assert!(
                matches!(err, MoltError::AlreadyApproved(got) if got == id),
                "unexpected: {err:?}"
            );
        }
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.applied.is_empty(), "2-of-3 never applies on one member");
        assert_eq!(snap.pending.len(), 1);
        assert_eq!(
            snap.pending[0].approvals, 1,
            "exactly this node's own approval, nothing invented"
        );
        assert!(snap.pending[0].approved_by_me);
    });
}

/// The explicit-vote twin: without self-cosign the FIRST `Approve` is
/// this node's real vote and is recorded; the second is the refused
/// simulation. The votes row attributes only what is known — me.
#[test]
fn second_local_approval_is_refused_without_chain_governance() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"t"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("my own first approval is real");
        let err = w
            .execute(Command::Approve { proposal: id })
            .await
            .expect_err("no second local approval");
        assert!(
            matches!(err, MoltError::AlreadyApproved(got) if got == id),
            "unexpected: {err:?}"
        );
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.applied.is_empty());
        assert_eq!(snap.pending[0].approvals, 1);
        // honest attribution: my vote is mine, the peers stay open
        for v in &snap.pending[0].votes {
            let expect = if v.member == "me" {
                molt_core::VoteState::Approved
            } else {
                molt_core::VoteState::Open
            };
            assert_eq!(v.vote, expect, "stance of {}", v.member);
        }
    });
}

/// The open-time crash recovery must not resurrect the simulation: a
/// legacy log whose counter reached a threshold > 1 did so on invented
/// peer approvals, and minting a fresh `Applied` from that count would
/// fake a threshold decision no member made. Such proposals stay
/// pending (decline is the only exit).
#[test]
fn recovery_never_applies_from_simulated_counts() {
    let mut st = plain_state(); // 2-of-3 demo config
    let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: by.to_string(),
        body,
    };
    st.apply(&e(
        1,
        "me",
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"t"}),
        },
    ));
    // a legacy pre-chain log: two counted approvals (the second was the
    // simulation), crash before the Applied frame
    for seq in [2, 3] {
        st.apply(&e(
            seq,
            "me",
            molt_core::WorkspaceEvent::Approved {
                id: molt_core::ProposalId(1),
                by: "me".to_string(),
                height: 0,
                sig: String::new(),
            },
        ));
    }
    st.recover_pending_applies();
    let snap = st.snapshot(Surface::Memory, None, None);
    assert!(snap.applied.is_empty(), "no apply on invented peer counts");
    assert_eq!(snap.pending.len(), 1, "the legacy proposal stays pending");
}

/// The honest twin: at threshold 1 the one recorded vote is the local
/// operator's real decision, so a crash between the `Approved` frame
/// and its `Applied` frame recovers into the applied state at open.
#[test]
fn recovery_completes_a_real_single_operator_decision() {
    let mut st = plain_state();
    st.config.threshold = 1;
    let e = |seq: u64, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: "me".to_string(),
        body,
    };
    st.apply(&e(
        1,
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(1),
            surface: Surface::Memory,
            payload: json!({"op":"add_note","title":"t"}),
        },
    ));
    st.apply(&e(
        2,
        molt_core::WorkspaceEvent::Approved {
            id: molt_core::ProposalId(1),
            by: "me".to_string(),
            height: 0,
            sig: String::new(),
        },
    ));
    st.recover_pending_applies();
    let snap = st.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied.len(), 1, "my one real vote recovers to applied");
    assert!(snap.pending.is_empty());
}

/// The solo boot group (1-of-1) is REAL governance, not a simulation:
/// the only member's own self-cosigned approval meets the threshold,
/// so a proposal applies through the same honest single-operator path.
#[test]
fn solo_boot_group_runs_real_one_of_one_governance() {
    rt().block_on(async {
        let w = spawn(GroupConfig::solo(), SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"solo"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        let snap = read_surface(&w, Surface::Memory).await;
        assert_eq!(
            snap.applied.len(),
            1,
            "the sole member's own approval meets threshold 1"
        );
        assert!(snap.pending.is_empty());
        // a late vote on the decided proposal names the terminal state
        let err = w
            .execute(Command::Approve { proposal: id })
            .await
            .expect_err("the vote is decided");
        assert!(
            matches!(err, MoltError::AlreadyTerminal(got, _) if got == id),
            "unexpected: {err:?}"
        );
    });
}

/// The status summary carries the founding date (the genesis envelope's
/// timestamp — real on replayed workspaces, 0 on the sessionless demo)
/// and the REAL activity trio: nobody in the demo boot group has ever
/// been seen on the wire, so only the local member counts anywhere.
#[test]
fn status_carries_founding_date_and_activity() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        match w.execute(Command::Status).await.expect("status") {
            Reply::Status(st) => {
                assert_eq!(st.founded_ts, 0, "the demo group has no genesis event");
                assert_eq!(
                    st.active_7d, 1,
                    "honest presence: never-seen peers count nowhere - only the local member"
                );
                assert!(st.active_1h <= st.active_24h && st.active_24h <= st.active_7d);
            }
            other => panic!("unexpected: {other:?}"),
        }
    });
}

/// The pending cards' "Ist-Stand / Soll-Stand" pair: an Organization
/// edit proposal exposes what the state is now (from the genesis
/// replica) and what the change would make it (the payload's `value`).
/// Display data, never consensus input — empty when unknown.
#[test]
fn org_pending_cards_carry_current_and_proposed_state() {
    let eff = |image: &str| proposals::OrgEffective {
        name: "Guild".into(),
        agenda: "alte Satzung".into(),
        retention_days: 7,
        image: image.to_string(),
        relays: String::new(),
        features: String::new(),
    };
    let rec = |surface: Surface, op: &str, value: &str| molt_core::ProposalRecord {
        surface,
        payload: json!({"op": op, "title": "t", "value": value}),
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
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_charter", "neue Satzung")
        ),
        ("alte Satzung".to_string(), "neue Satzung".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_name", "New Guild")
        ),
        ("Guild".to_string(), "New Guild".to_string())
    );
    // the image ops carry the current image reference as their Ist-Stand
    // ("" while none is set → the UI hides the empty line)
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_image", "~/logo.png")
        ),
        (String::new(), "~/logo.png".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff("/tmp/old.png"),
            &rec(Surface::Organization, "set_image", "~/logo.png")
        ),
        ("/tmp/old.png".to_string(), "~/logo.png".to_string())
    );
    assert_eq!(
        proposals::change_summary(
            &eff("/tmp/old.png"),
            &rec(Surface::Organization, "remove_image", "")
        ),
        ("/tmp/old.png".to_string(), String::new())
    );
    // a non-organization proposal exposes no pair beyond its value
    assert_eq!(
        proposals::change_summary(&eff(""), &rec(Surface::Memory, "add_note", "")),
        (String::new(), String::new())
    );
    // the chat-retention Ist-Stand is a MACHINE value (L10): the unit
    // renders in the frontends, per language; a legacy "14 days"
    // payload rides through untouched (the parser eats it)
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "set_chat_retention", "14 days")
        ),
        ("7".to_string(), "14 days".to_string())
    );
    // ops are free-form wire strings, so an older log may carry one this
    // build doesn't know (e.g. the retired plugin vocabulary): tolerated,
    // the Ist-Stand simply stays empty — never a rejection
    assert_eq!(
        proposals::change_summary(
            &eff(""),
            &rec(Surface::Organization, "enable_plugin", "calendar")
        ),
        (String::new(), "calendar".to_string())
    );
}

/// The republic's effective display identity is a fold of the applied
/// Organization log over the genesis: an applied `set_name` /
/// `set_charter` / `set_chat_retention` actually changes what every
/// reader sees (`StatusView.name/agenda/chat_retention_days`), and the
/// pending cards carry the EFFECTIVE state as their Ist-Stand. The
/// genesis itself stays immutable — it is only the fold's floor.
#[test]
fn effective_identity_follows_the_applied_org_ops() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: this node's own single approval
        // honestly applies each change (no peer is counted for)
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let status = |w: &WalletHandle| {
            let w = w.clone();
            async move {
                match w.execute(Command::Status).await.expect("status") {
                    Reply::Status(st) => st,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let propose = |op: &'static str, value: &'static str| {
            let w = w.clone();
            async move {
                let payload = json!({"op": op, "title": "t", "value": value});
                match w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id, .. } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let st = status(&w).await;
        assert_eq!(st.name, "", "a demo workspace has no genesis name");
        assert_eq!(st.agenda, "");
        assert_eq!(st.chat_retention_days, 7, "the default window is 7 days");
        for (op, value) in [
            ("set_name", "Neue Gilde"),
            ("set_charter", "wir bauen echte dinge"),
            ("set_chat_retention", "14 days"),
        ] {
            let id = propose(op, value).await;
            w.execute(Command::Approve { proposal: id }).await.expect("approve");
        }
        let st = status(&w).await;
        assert_eq!(st.name, "Neue Gilde");
        assert_eq!(st.agenda, "wir bauen echte dinge");
        assert_eq!(st.chat_retention_days, 14);
        // a follow-up proposal shows the EFFECTIVE state as Ist-Stand
        let _next = propose("set_name", "Dritte Gilde").await;
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].current, "Neue Gilde");
        assert_eq!(pending[0].proposed, "Dritte Gilde");
        // a bare number parses as days too
        let id = propose("set_chat_retention", "21").await;
        w.execute(Command::Approve { proposal: id }).await.expect("approve");
        assert_eq!(status(&w).await.chat_retention_days, 21);
        // nonsense is refused at propose time — an unparseable window
        // must never reach the applied log
        for bad in ["bald", "", "0 days", "9999 days"] {
            let err = w
                .execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({"op": "set_chat_retention", "title": "t", "value": bad}),
                })
                .await
                .expect_err("an unparseable retention window is refused");
            assert!(
                matches!(err, MoltError::BadPayload(_)),
                "unexpected error for {bad:?}: {err:?}"
            );
        }
        // an empty name is refused too (the fold must never go blank)
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_name", "title": "t", "value": "  "}),
            })
            .await
            .expect_err("an empty name is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// WP1 (governance follow-ups): the read contract carries a parallel id
/// track — `SurfaceSnapshot.applied_ids` is positionally parallel to
/// `applied` and names the proposal each entry came from. `None` =
/// origin unknown (chat rows, legacy dumps). The payloads themselves
/// stay byte-identical — the UI fate probe and MCP readers compare them.
#[test]
fn applied_entries_carry_their_proposal_id() {
    let mut st = plain_state();
    let e = |seq: u64, by: &str, body: molt_core::WorkspaceEvent| molt_core::EventEnvelope { prev_seq: 0,
        seq,
        ts: 100 + seq,
        by: by.to_string(),
        body,
    };
    let payload = json!({"op": "add_note", "title": "minutes"});
    st.apply(&e(
        1,
        "petra",
        molt_core::WorkspaceEvent::Proposed {
            id: molt_core::ProposalId(4),
            surface: Surface::Memory,
            payload: payload.clone(),
        },
    ));
    st.apply(&e(
        2,
        "walter",
        molt_core::WorkspaceEvent::Applied {
            id: molt_core::ProposalId(4),
        },
    ));
    let snap = st.snapshot(Surface::Memory, None, None);
    assert_eq!(snap.applied, vec![payload.clone()], "payload untouched");
    assert_eq!(
        snap.applied_ids,
        vec![Some(4)],
        "the applied entry knows the proposal it came from"
    );
    // chat rows have no proposal origin: same length, all None
    st.apply(&e(
        3,
        "petra",
        // ts 0 = unknown age: always inside the retention read window
        molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
            molt_core::MessageId([7u8; 16]),
            "petra",
            "gm",
            0,
        )),
    ));
    let chat = st.snapshot(Surface::Chat, None, None);
    assert_eq!(chat.applied.len(), 1);
    assert_eq!(chat.applied_ids, vec![None]);
    // a NEW dump round-trips the id track…
    let dump = st.snapshot_now().state;
    let mut st2 = plain_state();
    st2.restore_dump(dump.clone());
    assert_eq!(
        st2.snapshot(Surface::Memory, None, None).applied_ids,
        vec![Some(4)]
    );
    // …a LEGACY dump (a pre-id writer: the field is absent) restores the
    // payloads unchanged with unknown origin
    let mut v = serde_json::to_value(&dump).expect("dump serializes");
    v.as_object_mut().expect("a JSON object").remove("applied_ids");
    let legacy: molt_core::EngineStateDump =
        serde_json::from_value(v).expect("legacy dump deserializes");
    let mut st3 = plain_state();
    st3.restore_dump(legacy);
    let restored = st3.snapshot(Surface::Memory, None, None);
    assert_eq!(restored.applied, vec![payload], "payloads survive untouched");
    assert_eq!(restored.applied_ids, vec![None], "unknown origin stays honest");
}

/// The republic's current image is derived from the applied
/// Organization log: the last applied `set_image` wins, an applied
/// `remove_image` clears it — and the pending image cards carry it as
/// their Ist-Stand. A `set_image` now CARRIES the bytes (base64 in the
/// payload — sign-what-you-see: members vote on the actual image); on
/// a session-only workspace (no storage dir to materialize a logo
/// file into) the reference falls back to the proposed display value.
#[test]
fn current_image_follows_the_applied_org_ops() {
    use base64::Engine as _;
    rt().block_on(async {
        // 1-of-3, no self-cosign: this node's own single approval
        // honestly applies each change (no peer is counted for)
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let status = |w: &WalletHandle| {
            let w = w.clone();
            async move {
                match w.execute(Command::Status).await.expect("status") {
                    Reply::Status(st) => st,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        // a real 2x2 PNG — since WP3 the bytes must decode as a picture
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==".to_string();
        let propose = |op: &'static str, value: &'static str, with_bytes: bool| {
            let w = w.clone();
            let b64 = b64.clone();
            async move {
                let mut payload = json!({"op": op, "title": "t", "value": value});
                if with_bytes {
                    payload["bytes_b64"] = json!(b64);
                }
                match w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id, .. } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        assert_eq!(status(&w).await.image, "", "no image before any change");
        // 1-of-3: this node's own approval applies the change
        let id = propose("set_image", "team.png", true).await;
        w.execute(Command::Approve { proposal: id }).await.expect("approve");
        assert_eq!(status(&w).await.image, "team.png");
        // a follow-up image proposal shows the applied state as Ist-Stand
        let next = propose("set_image", "new.png", true).await;
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].current, "team.png");
        assert_eq!(pending[0].proposed, "new.png");
        w.execute(Command::Approve { proposal: next }).await.expect("approve");
        assert_eq!(status(&w).await.image, "new.png", "last applied wins");
        // an applied remove_image clears the state again
        let rm = propose("remove_image", "", false).await;
        w.execute(Command::Approve { proposal: rm }).await.expect("approve");
        assert_eq!(status(&w).await.image, "");
        // a set_image without the actual bytes is refused — the mock
        // path-reference era is over (nothing real could be applied)
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "title": "t", "value": "x.png"}),
            })
            .await
            .expect_err("a set_image without bytes is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // bytes beyond what the transport can carry are refused with a
        // clear error — the ceiling is DERIVED from the publish budget
        // (`proposals::size_gate_tests` pins the derivation), and 256 KiB
        // is comfortably past it
        let big = base64::engine::general_purpose::STANDARD
            .encode(vec![0u8; 256 * 1024]);
        let err = w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op": "set_image", "title": "t", "value": "big.png", "bytes_b64": big}),
            })
            .await
            .expect_err("an oversized image is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// WP3: a `set_image` proposal must carry DECODABLE bytes — a member
/// asked to sign-what-they-see must be able to see it. The engine
/// sniffs format + header dimensions (never a full decode — decode
/// bombs); real 2×2 fixtures of every picker format pass, garbage and
/// a dimension bomb are refused with an honest error.
#[test]
fn an_undecodable_set_image_proposal_is_refused() {
    use base64::Engine as _;
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let propose = |b64: String| {
            let w = w.clone();
            async move {
                w.execute(Command::Propose {
                    surface: Surface::Organization,
                    payload: json!({
                        "op": "set_image", "value": "x.png", "bytes_b64": b64,
                    }),
                })
                .await
            }
        };
        // garbage bytes: refused with a clear error
        let garbage =
            base64::engine::general_purpose::STANDARD.encode(b"definitely not an image");
        let err = propose(garbage).await.expect_err("garbage is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // a dimension bomb: a valid BMP HEADER declaring 20000x20000 —
        // the sniff reads only the header and refuses before any decode
        let bomb = base64::engine::general_purpose::STANDARD
            .encode(tiny_bmp_header(20_000, 20_000));
        let err = propose(bomb).await.expect_err("a dimension bomb is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
        // real minimal raster files (2x2, PIL-generated — the molt-ui
        // preview fixtures) pass for every remaining picker format
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
        for (fmt, b64) in [("png", png.to_string()), ("webp", webp.to_string())] {
            propose(b64).await.unwrap_or_else(|e| panic!("{fmt} must pass: {e:?}"));
        }
        // L1 (decided 2026-08-16): SVG is refused with its OWN reason —
        // the prefix sniff accepted any <svg/<?xml text unvetted
        // (billion-laughs class), and a structural vetting would be a
        // hand-rolled parser gate (the URL-parser lesson). Applied
        // legacy SVG logos keep rendering; this is propose/wire-only.
        let svg = base64::engine::general_purpose::STANDARD.encode(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
        );
        let err = propose(svg).await.expect_err("svg is refused");
        assert!(
            format!("{err:?}").contains("svg is not accepted"),
            "the refusal names the reason: {err:?}"
        );
        let bomb = base64::engine::general_purpose::STANDARD.encode(
            r#"<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol">]><svg>&lol;</svg>"#,
        );
        let err = propose(bomb).await.expect_err("an xml entity bomb is refused");
        assert!(matches!(err, MoltError::BadPayload(_)), "unexpected: {err:?}");
    });
}

/// Organization is a gated surface like the others: charter / name /
/// logo / retention changes go through propose → threshold → applied — and
/// because the MCP `propose` tool derives its surface list from
/// `is_gated`, the GUI edit modals and an MCP agent drive the SAME path.
#[test]
fn organization_changes_are_gated_proposals() {
    rt().block_on(async {
        // 1-of-3, no self-cosign: propose leaves the vote genuinely
        // open, this node's own approval honestly applies it
        let cfg = GroupConfig {
            threshold: 1,
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Organization,
                payload: json!({"op":"set_charter","title":"Charter ändern","value":"neue Satzung"}),
            })
            .await
            .expect("propose on organization")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        // the pending view carries the Soll-Stand (the payload's value);
        // the Ist-Stand stays empty on a demo workspace (no genesis)
        let pending = read_surface(&w, Surface::Organization).await.pending;
        assert_eq!(pending[0].proposed, "neue Satzung");
        assert_eq!(pending[0].current, "");
        // threshold 1: this node's own approval applies the change
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        let snap = read_surface(&w, Surface::Organization).await;
        assert!(snap.gated, "organization is threshold-gated");
        assert_eq!(snap.applied.len(), 1, "applied at threshold");
        assert!(snap.pending.is_empty());
        // an op this build doesn't know still proposes: ops are free-form
        // wire strings (an MCP agent or an older/newer build may mint
        // one), so the validator only vets the ops it understands
        w.execute(Command::Propose {
            surface: Surface::Organization,
            payload: json!({"op":"enable_plugin","title":"t","value":"calendar"}),
        })
        .await
        .expect("an unknown org op is tolerated, not rejected");
    });
}

/// The pending cards render a voting row: per-member stance in roster
/// order. On the single-operator path the only attributable vote is
/// this node's own — my approval flips exactly my pill, every peer
/// honestly stays open.
#[test]
fn pending_views_carry_per_member_votes() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let roster = cfg.members.clone();
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"minutes"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        // fresh proposal, no self-cosign: the whole roster is open
        let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
        assert_eq!(
            votes.iter().map(|v| v.member.clone()).collect::<Vec<_>>(),
            roster,
            "one entry per roster member, in roster order"
        );
        assert!(votes.iter().all(|v| v.vote == molt_core::VoteState::Open));
        // my approval flips exactly my entry (the demo member is "me")
        w.execute(Command::Approve { proposal: id })
            .await
            .expect("approve");
        let votes = &read_surface(&w, Surface::Memory).await.pending[0].votes;
        for v in votes {
            let expect = if v.member == "me" {
                molt_core::VoteState::Approved
            } else {
                molt_core::VoteState::Open
            };
            assert_eq!(v.vote, expect, "stance of {}", v.member);
        }
    });
}

/// The read contract splits a surface's open governance by the reader:
/// a pending proposal says whether THIS node already approved it
/// (`approved_by_me`), and declined proposals count into `denied` —
/// the Organization → Status approvals table renders exactly these.
#[test]
fn pending_views_split_by_my_vote_and_count_denied() {
    rt().block_on(async {
        // no self-cosign: a fresh proposal starts with zero approvals,
        // so it genuinely waits on this node's vote
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let propose = |title: &str| {
            let w = &w;
            let payload = json!({"op":"add_note","title":title});
            async move {
                match w
                    .execute(Command::Propose {
                        surface: Surface::Memory,
                        payload,
                    })
                    .await
                    .expect("propose")
                {
                    Reply::Proposed { id, .. } => id,
                    other => panic!("unexpected: {other:?}"),
                }
            }
        };
        let waiting_on_me = propose("waiting").await;
        let voted = propose("voted").await;
        let declined = propose("declined").await;
        // one approval of two: still pending, but no longer waiting on me
        w.execute(Command::Approve { proposal: voted })
            .await
            .expect("approve");
        w.execute(Command::Decline { proposal: declined })
            .await
            .expect("decline");
        let snap = read_surface(&w, Surface::Memory).await;
        assert_eq!(snap.pending.len(), 2);
        let by_id = |id| {
            snap.pending
                .iter()
                .find(|p| p.id == id)
                .expect("pending view")
        };
        assert!(
            !by_id(waiting_on_me).approved_by_me,
            "an untouched proposal waits on this node's vote"
        );
        assert!(
            by_id(voted).approved_by_me,
            "the own approval must reflect in the pending view"
        );
        assert_eq!(snap.denied, 1, "the declined proposal counts as denied");
    });
}

/// A declined proposal leaves `pending` and surfaces in the snapshot's
/// `declined` list — with who declined and when (the envelope ts the
/// GUI's retention window filters on), and the decliner's stance marked
/// in the votes row. The Organization → Declined view renders exactly
/// this projection.
#[test]
fn declined_proposals_surface_with_decliner_and_timestamp() {
    rt().block_on(async {
        let cfg = GroupConfig {
            self_cosign: false,
            ..GroupConfig::demo()
        };
        let w = spawn(cfg, SessionView::default());
        let id = match w
            .execute(Command::Propose {
                surface: Surface::Memory,
                payload: json!({"op":"add_note","title":"nope"}),
            })
            .await
            .expect("propose")
        {
            Reply::Proposed { id, .. } => id,
            other => panic!("unexpected: {other:?}"),
        };
        w.execute(Command::Decline { proposal: id })
            .await
            .expect("decline");
        let snap = read_surface(&w, Surface::Memory).await;
        assert!(snap.pending.is_empty(), "a decline leaves pending");
        assert_eq!(snap.denied, 1, "the count stays for the status strip");
        assert_eq!(snap.declined.len(), 1, "the declined view is exposed");
        let v = &snap.declined[0];
        assert_eq!(v.id, id);
        assert_eq!(v.state, molt_core::ProposalState::Rejected);
        assert_eq!(v.declined_by, "me", "the decliner is named");
        assert!(v.declined_at > 0, "the decline carries its envelope ts");
        let mine = v
            .votes
            .iter()
            .find(|x| x.member == "me")
            .expect("my roster row");
        assert_eq!(
            mine.vote,
            molt_core::VoteState::Declined,
            "the votes row marks the decliner"
        );
    });
}
