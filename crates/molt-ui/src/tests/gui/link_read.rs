// SPDX-License-Identifier: GPL-3.0-or-later
//! The Restore wizard's one link field (welcome_rework.md): the engine
//! says what the link is (`docs/web_gui.md` §6.4), the window shows it.

use super::*;
use crate::actions::ritual::{land_link, link_failed, read_link};

/// The two link shapes are rendered by the ENGINE's own `render()`,
/// never hand-written here: a hand-built string pins the test's idea of
/// the format, and the day the real one changes the test keeps passing
/// while the panel stops recognizing anything.
/// A real x-only anchor - the handover encoders validate the key, so a
/// made-up hex string cannot stand in for one.
fn anchor(seed: u8) -> String {
    molt_net::nostr_identity(&[seed; 32], "fixture").1
}

pub(super) fn invite_link() -> String {
    molt_engine::FoundingInvite {
        info: molt_core::InviteInfo {
            republic: "Chess Club".to_string(),
            threshold: 2,
            members: 3,
            inviter: "walter".to_string(),
            ticket: "a".repeat(64),
        },
        handover: molt_net::invite::InviteHandoverV2 {
            seat: 1,
            ticket: "a".repeat(64),
            npub: anchor(1),
            relays: vec!["ws://127.0.0.1:7777".to_string()],
        },
    }
    .render()
    .expect("the engine renders its own link")
}

fn recovery_link() -> String {
    molt_engine::RecoveryInvite {
        consent_anchor: None,
        republic: "Chess Club".to_string(),
        member: "petra".to_string(),
        ticket: "c".repeat(64),
        server: String::new(),
        queue_id: String::new(),
        wrap: String::new(),
        republic_id: "d".repeat(64),
        handover: Some(molt_net::invite::RecoveryHandoverV2 {
            consent_anchor: None,
            identity_pk: String::new(),
            ticket: "c".repeat(64),
            npub: anchor(2),
            relays: vec!["ws://127.0.0.1:7777".to_string()],
            republic_id: "d".repeat(64),
        }),
    }
    .render()
}

/// A window with the link panel wired to a real engine.
struct Panel {
    ui: AppWindow,
    cx: Ctx,
    rt: tokio::runtime::Runtime,
    _tmp: tempfile::TempDir,
}

fn panel() -> Panel {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let ui = AppWindow::new().expect("headless window");
    let cx = {
        let _guard = rt.enter();
        let (w, _) = node_with_chat(tmp.path());
        Ctx {
            spawner: rt.handle().clone().into(),
            wallet: w.into(),
            weak: ui.as_weak(),
            last_settings: Arc::new(Mutex::new(None)),
            chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
        }
    };
    crate::actions::ritual::wire(&ui, &cx);
    // this node can dial the fixture links' relay, so only the link decides
    cx.chat_ui.lock().expect("ui state").set_link_pool(
        vec![molt_core::relay::RelayEntry { url: "ws://127.0.0.1:7777".to_string(), confirmed: true }],
        true,
    );
    // a join needs a name
    ui.set_jw_member("petra".into());
    Panel { ui, cx, rt, _tmp: tmp }
}

impl Panel {
    /// The engine's answer for `link`, fetched the way the window does.
    fn answer(&self, link: &str) -> molt_core::LinkView {
        self.rt.block_on(read_link(&self.cx.wallet, link.trim().to_string())).expect("a verdict")
    }

    /// Put `link` in the field, as typing or a paste does. The window's
    /// own read is in flight afterwards; its answer is NOT landed (the
    /// headless backend never drains the event-loop queue).
    fn enter(&self, link: &str) {
        self.ui.set_rv_link(link.into());
        self.ui.invoke_rw_link_changed(link.into());
    }

    fn land(&self, view: molt_core::LinkView) {
        land_link(&self.ui, &self.cx.chat_ui, view);
    }

    /// Enter `link` and land the engine's answer for it.
    fn judge(&self, link: &str) -> LinkPreview {
        self.enter(link);
        self.land(self.answer(link));
        self.ui.get_rv_kind()
    }

    fn armed(&self) -> bool {
        self.ui.invoke_link_armed()
    }

    fn relays(&self) -> Vec<String> {
        self.ui.get_rw_relays().iter().map(|r| r.url.to_string()).collect()
    }
}

/// One field, two flows: an invite link asks for a NAME and joins, a
/// recovery link brings its own seat and needs the PHRASE. Getting this
/// wrong sends someone through the founding ritual to recover a seat
/// they already hold, so it is pinned rather than eyeballed.
#[test]
fn one_link_field_tells_a_join_from_a_recovery() {
    let p = panel();
    let v = p.judge(&invite_link());
    assert_eq!(
        (v.kind, v.republic.as_str(), v.who.as_str()),
        (1, "Chess Club", "walter"),
        "a founding invite routes to the join"
    );
    assert!(p.armed(), "a named joiner holding an invite can continue");
    assert_eq!(p.relays(), ["ws://127.0.0.1:7777"], "the panel lists the link's relays");

    let v = p.judge(&recovery_link());
    assert_eq!(
        (v.kind, v.republic.as_str(), v.who.as_str()),
        (2, "Chess Club", "petra"),
        "a recovery link routes to the ritual, and names its own seat"
    );
    assert!(p.armed());

    // whitespace is what a paste actually carries
    let v = p.judge(&format!("  {}\n", invite_link()));
    assert_eq!((v.kind, v.who.as_str()), (1, "walter"));
    assert!(p.armed());
}

/// Everything else arms nothing. A PREVIEW-only invite link is the
/// interesting case: it parses as a human-readable invite and carries no
/// transport handover at all, so a panel that armed on "looks like an
/// invite" would start a join that cannot reach anybody.
#[test]
fn a_link_that_cannot_act_arms_nothing() {
    let p = panel();
    let preview = "molt://invite/Chess-Club/2of3/walter/k9x2m4q7aa";
    assert_eq!(
        p.judge(preview).kind,
        0,
        "a preview link has no transport handover - nothing can be done with it"
    );
    assert!(!p.armed());
    let damaged = format!("{}zz", recovery_link());
    assert_eq!(p.judge(&damaged).kind, 0, "a damaged recovery handover is not an actionable link");
    assert!(!p.armed());
    let damaged = format!("{}zz", invite_link());
    assert_eq!(p.judge(&damaged).kind, 0, "a damaged invite handover is not an actionable link");
    assert!(!p.armed());
    for junk in ["hello", "molt://", "molt://invite/", "https://example.com"] {
        assert_eq!(p.judge(junk).kind, 0, "junk: {junk:?}");
        assert!(p.ui.get_rv_judged(), "junk: {junk:?}");
        assert!(!p.armed(), "junk: {junk:?}");
        assert!(p.relays().is_empty(), "junk: {junk:?}");
    }
}

/// Blank text is never sent to the engine, so the window judges it by
/// itself: unrecognized, at once - not the silence of a read in flight.
#[test]
fn a_blank_link_is_judged_without_a_read() {
    let p = panel();
    assert_eq!(p.judge(&invite_link()).kind, 1);
    for blank in ["   ", " \n", "\t"] {
        p.enter(blank);
        assert!(p.ui.get_rv_judged(), "blank: {blank:?}");
        assert_eq!(p.ui.get_rv_kind().kind, 0, "blank: {blank:?}");
        assert!(!p.armed(), "blank: {blank:?}");
        assert!(p.relays().is_empty(), "blank: {blank:?}");
    }
    p.enter("");
    assert!(p.ui.get_rv_judged());
    assert!(!p.armed());
}

/// A read that fails says so once: not for a link the field no longer
/// holds, and not again per keystroke while the fault lasts.
#[test]
fn a_failed_read_toasts_once_and_arms_nothing() {
    let p = panel();
    let gone = molt_core::MoltError::BadPayload("gone".to_string());
    let toast = |p: &Panel| p.ui.get_toast_text().to_string();

    // a failure for a link the field has left is dropped
    p.enter("hello");
    link_failed(&p.ui, &p.cx.chat_ui, "hell", &gone);
    assert_eq!(toast(&p), "", "a stale failure is not news");

    link_failed(&p.ui, &p.cx.chat_ui, "hello", &gone);
    assert!(toast(&p).contains("gone"), "{:?}", toast(&p));
    assert!(!p.ui.get_rv_judged(), "a failed read is no verdict");
    assert!(!p.armed());

    // the next keystroke fails the same way: no second toast
    p.ui.invoke_show_toast("".into());
    p.enter("hello!");
    link_failed(&p.ui, &p.cx.chat_ui, "hello!", &gone);
    assert_eq!(toast(&p), "", "one toast per outage");

    // an answer ends the outage; the next failure is news again
    let invite = invite_link();
    assert_eq!(p.judge(&invite).kind, 1);
    p.enter("hello");
    link_failed(&p.ui, &p.cx.chat_ui, "hello", &gone);
    assert!(toast(&p).contains("gone"));
}

/// **§6.4.** The verdict arrives after the link: the moment the link
/// changes the old verdict is gone, and an answer for a link the field
/// no longer holds is dropped - it must never arm join or recover.
#[test]
fn a_stale_verdict_never_arms_the_panel() {
    let p = panel();
    let (invite, recovery) = (invite_link(), recovery_link());
    assert_eq!(p.judge(&invite).kind, 1);
    assert!(p.armed());
    let for_invite = p.answer(&invite);

    // the link changes: nothing is armed BEFORE any answer comes back
    p.enter("hello");
    assert_eq!(p.ui.get_rv_verdict(), LinkPreview::default(), "the old verdict leaves with the old link");
    assert_eq!(p.ui.get_rv_kind().kind, 0);
    assert!(!p.armed());
    assert!(p.relays().is_empty(), "and so do its relays");

    // the invite's answer lands late
    p.land(for_invite.clone());
    assert_eq!(p.ui.get_rv_verdict(), LinkPreview::default(), "a late answer is dropped");
    assert!(!p.armed());
    assert!(p.relays().is_empty(), "its relays with it");

    // the other order: a recovery link in the field, the invite's answer late
    p.enter(&recovery);
    p.land(for_invite.clone());
    assert_eq!(p.ui.get_rv_kind().kind, 0, "an invite's answer does not judge a recovery link");
    assert!(!p.armed());
    p.land(p.answer(&recovery));
    assert_eq!(p.ui.get_rv_kind().kind, 2);
    assert!(p.armed());

    // the window holds the same line by itself: a verdict for another
    // text than the field shows judges nothing
    p.ui.set_rv_verdict(LinkPreview { link: invite.as_str().into(), kind: 1, republic: "R".into(), who: "w".into() });
    assert_eq!(p.ui.get_rv_kind().kind, 0);
    assert!(!p.armed());

    // back to the invite: its answer is current again
    p.enter(&invite);
    assert!(!p.armed(), "not before the answer");
    p.land(for_invite);
    assert_eq!(p.ui.get_rv_kind().kind, 1);
    assert!(p.armed());
}

/// A kind this window does not know arms nothing.
#[test]
fn an_unknown_kind_arms_nothing() {
    let p = panel();
    p.enter("molt://later/x");
    p.land(molt_core::LinkView { link: "molt://later/x".to_string(), kind: 3, ..molt_core::LinkView::default() });
    assert_eq!(p.ui.get_rv_kind().kind, 0);
    assert!(!p.armed());
}

/// The founder's seat rows offer a link to copy only when the engine
/// says it can be joined with; the window does not parse it.
#[test]
fn a_seat_row_offers_only_a_joinable_link() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless window");
    let seat = |link: &str, joinable| molt_core::RitualSeatView {
        link: link.to_string(),
        joinable,
        ..molt_core::RitualSeatView::default()
    };
    let mut sv = SessionView::default();
    sv.create.member = "walter".to_string();
    sv.create.seats = vec![seat("molt://invite/ready", true), seat("molt://invite/preview", false)];
    apply_runs(&ui, &sv);
    let details: Vec<String> = ui.get_cw_seats().iter().map(|s| s.detail.to_string()).collect();
    assert_eq!(details[1..], ["molt://invite/ready", ""], "{details:?}");
}
