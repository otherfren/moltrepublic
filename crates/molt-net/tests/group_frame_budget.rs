// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! **What a kind-445 can actually carry — measured, not estimated.**
//!
//! `ORG_IMAGE_MAX_BYTES` is 256 KiB and `DEFAULT_SIZE_BUDGET` is 128 KiB for
//! the whole websocket message. The two numbers were set independently and
//! never reconciled, and the gap is not academic: `RelayRuntime::publish`
//! measures the framed `["EVENT",{…}]` and refuses over budget **locally**,
//! before any relay is contacted — so the refusal is deterministic, the
//! outbox's three retries are futile by construction, and the cursor then
//! holds at that envelope forever (`group_runtime.rs`, deliberately: nothing
//! recovers a skipped envelope). One oversized image wedges everything the
//! node writes after it, across restarts.
//!
//! So the propose-time cap has to be DERIVED from this budget rather than
//! chosen. This measures the derivation, through the real pipeline: the
//! payload's base64, the block JSON, the MLS ciphertext, `seal_outer`'s
//! base64, and the event framing that `publish` actually counts.

use molt_core::chain::{ChainBlock, ChainChange};
use molt_core::{EventEnvelope, RosterAttestation, WorkspaceEvent};
use molt_net::envelope::seal_outer;
use molt_net::mls::MlsMember;
use molt_net::relay_runtime::DEFAULT_SIZE_BUDGET;
use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};

/// The wire cost of ONE `set_image` block of `raw` decoded bytes, measured
/// exactly where `RelayRuntime::publish` measures it.
fn wire_cost(mls: &mut MlsMember, raw: usize) -> usize {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![0x89u8; raw]);
    let block = ChainBlock {
        height: 7,
        prev: "cd".repeat(32),
        change: ChainChange::Applied {
            proposal_id: 7,
            surface: molt_core::Surface::Organization,
            payload: serde_json::json!({ "op": "set_image", "value": "logo.png", "bytes_b64": b64 }),
        },
        sigs: vec![
            RosterAttestation { member: "walter".to_string(), sig: "ab".repeat(64) },
            RosterAttestation { member: "petra".to_string(), sig: "cd".repeat(64) },
        ],
    };
    let env = EventEnvelope {
        prev_seq: 0,
        seq: 7,
        ts: 1_751_000_000,
        by: "walter".to_string(),
        body: WorkspaceEvent::Committed(block),
    };
    let plaintext = serde_json::to_vec(&env).expect("envelope serializes");
    let ciphertext = mls.encrypt(&plaintext).expect("mls encrypt");
    let exporter = mls.exporter_secret().expect("exporter");
    let content = seal_outer(&exporter, &ciphertext).expect("seal");
    let event = EventBuilder::new(Kind::Custom(molt_net::kinds::KIND_GROUP), content)
        .tag(Tag::parse(["h", &"ab".repeat(32)]).expect("h tag"))
        .sign_with_keys(&Keys::generate())
        .expect("sign");
    nostr::ClientMessage::event(event).as_json().len()
}

/// `cost / raw` in PER MILLE — integer arithmetic, because this workspace
/// lints float maths and a ratio needs no floats to be readable (1800 = x1.8).
fn permille(cost: usize, raw: usize) -> usize {
    cost * 1000 / raw
}

fn group_of_one() -> MlsMember {
    use ed25519_dalek::SigningKey;
    let mut m = MlsMember::new(&SigningKey::from_bytes(&[1u8; 32]), "walter").expect("member");
    m.create_group().expect("group");
    m
}

/// **The number the propose-time cap must be derived from.**
///
/// Reported, then bracketed: the test asserts the ceiling is far below
/// `ORG_IMAGE_MAX_BYTES` (which is the finding) and above a floor that keeps
/// a usable logo possible (which is what makes the finding actionable rather
/// than a counsel of despair).
#[test]
fn the_publishable_image_ceiling_is_far_below_the_propose_time_cap() {
    let mut mls = group_of_one();
    let budget = usize::try_from(DEFAULT_SIZE_BUDGET).expect("budget fits");

    // walk upward in 4 KiB steps and report the last size that still fits
    let mut ceiling = 0usize;
    let mut cost_at_ceiling = 0usize;
    for kib in (4..=256).step_by(4) {
        let raw = kib * 1024;
        let cost = wire_cost(&mut mls, raw);
        if cost <= budget {
            ceiling = raw;
            cost_at_ceiling = cost;
        } else {
            eprintln!(
                "MEASURED first over-budget size: {kib} KiB image -> {cost} B event \
                 (budget {budget} B)"
            );
            break;
        }
    }
    let expansion = permille(cost_at_ceiling, ceiling);
    eprintln!(
        "MEASURED ceiling: {} KiB image -> {cost_at_ceiling} B event, \
         expansion x{}.{:03}, budget {budget} B",
        ceiling / 1024,
        expansion / 1000,
        expansion % 1000
    );

    let propose_cap = 256 * 1024;
    assert!(
        ceiling < propose_cap,
        "the measurement is the whole point: a {propose_cap}-byte image must NOT \
         be publishable, or there was nothing to reconcile"
    );
    // …and the gap is large, not marginal — a factor, not a rounding error
    assert!(
        ceiling * 2 < propose_cap,
        "ceiling {ceiling} B is within 2x of the {propose_cap} B propose cap — \
         re-read this test before trusting the conclusion"
    );
    // …while still leaving room for a real logo, so the fix is "lower the cap",
    // not "images cannot ride the chain at all"
    assert!(
        ceiling >= 16 * 1024,
        "only {ceiling} B fits — that is too small for a logo, and the answer \
         would have to be structural (a hash in the block, bytes elsewhere) \
         rather than a smaller cap"
    );
}

/// The expansion is the reason the two numbers cannot simply be equal: an
/// image is base64 in the payload and the sealed frame is base64 again, so
/// the wire cost is close to 16/9 of the decoded bytes before any framing.
#[test]
fn an_image_costs_about_twice_its_bytes_on_the_wire() {
    let mut mls = group_of_one();
    let raw = 32 * 1024;
    let cost = wire_cost(&mut mls, raw);
    let expansion = permille(cost, raw);
    eprintln!(
        "MEASURED expansion at 32 KiB: x{}.{:03} ({cost} B)",
        expansion / 1000,
        expansion % 1000
    );
    assert!(
        (1600..2400).contains(&expansion),
        "expansion {expansion} per mille is outside the double-base64 range \
         this reasoning assumes - the pipeline changed shape"
    );
}
