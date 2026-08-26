// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]
// The handwritten GUI logic casts small ints to Slint's `i32`, does float
// label math, and drives Slint APIs that return `Option`s we unwrap; the
// allows are scoped to this UI crate only, so the rest of the workspace
// keeps the strict posture. (Slint's GENERATED code lives in molt-ui-window
// with its own allow header.)
#![allow(
    clippy::as_conversions,
    clippy::unwrap_used,
    clippy::float_arithmetic,
    clippy::todo
)]

//! `molt-ui`: the GUI operator.
//!
//! This crate hosts the multi-stage front of the node — a first-run wizard
//! (create / open / join / restore), a shared completion screen, the main
//! surfaces view, and a settings panel. The settings are real (they persist
//! to the node's `config.toml` and mirror external edits of it); the
//! workspace lifecycles are real — create/open/join/close write to disk.
//!
//! The GUI is a **live-mirror of the engine's shared session**, not a holder of
//! its own state. Every action (navigate, switch language, save settings, finish
//! a wizard) is turned into a [`molt_core::Command`] on the shared
//! [`WalletHandle`]; a background task re-reads the session on each
//! [`molt_core::Event::SessionChanged`] and pushes it back into the Slint
//! properties. An MCP agent issuing the *same* commands drives this *same* state,
//! so the GUI and the MCP operator are co-equal — exactly as for the surfaces.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};


use molt_core::{Command, Event, SessionScope, SessionSettings};
use molt_engine::WalletHandle;
use slint::{Model, ModelRc, VecModel};
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

// The Slint-generated window (AppWindow, the Strings/Theme globals, every
// row struct) lives in its own crate as a compile-time firewall — see
// molt-ui-window's crate docs. The glob keeps this crate's code reading as
// if the module were still injected here.
pub use molt_ui_window::*;

/// The Restore wizard's one link field: which flow a pasted link arms.
pub use actions::ritual::{link_kind, LinkKind};

mod actions;
mod alerts;
mod channels;
mod chat_log;
mod i18n;
mod images;
mod labels;
mod mirror;
mod models;
mod net_tor;
mod patchview;
mod settings;
mod surfaces;
mod wiki;
mod wiki_bridge;

use alerts::*;
#[cfg(test)]
use channels::*;
#[cfg(test)]
use actions::relays::relay_add_check;
#[cfg(test)]
use chat_log::*;
use i18n::*;
#[cfg(test)]
use images::*;
#[cfg(test)]
use labels::*;
use mirror::*;
use net_tor::*;
use settings::*;
use surfaces::*;
use wiki_bridge::*;

/// Open the GUI and run the Slint event loop on the calling (main) thread.
///
/// `config_path` is shown in the settings panel as the location a real save
/// *would* target. `embedded_tor_available` is the compile-time truth of the
/// binary's `embedded-tor` feature (P3): when false, the tor-mode dropdown greys
/// its "embedded" row (the in-process arti dialer was not built in). Returns
/// when the window closes, or an error if the GUI cannot start (e.g. no display)
/// — in which case the caller falls back to headless.
pub fn run_app(
    wallet: WalletHandle,
    rt: Handle,
    config_path: PathBuf,
    embedded_tor_available: bool,
) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    // the settings footer shows the full config.toml location: directory
    // greyed, file name in text color. Absolutize a relative discovery path
    // (e.g. "config.toml" found in the cwd) so it reads as a full path.
    let abs = if config_path.is_absolute() {
        config_path.clone()
    } else {
        std::env::current_dir()
            .map(|d| d.join(&config_path))
            .unwrap_or_else(|_| config_path.clone())
    };
    let file = abs
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let dir = abs
        .parent()
        .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
        .unwrap_or_default();
    ui.set_config_dir(dir.into());
    ui.set_config_file(file.into());
    // Surface the compile-time embedded-tor availability into the tor-mode
    // dropdown's per-row enabled flags (a constant for the process lifetime).
    ui.set_tor_mode_enabled(ModelRc::new(VecModel::from(
        tor_mode_enabled(embedded_tor_available).to_vec(),
    )));

    // Clipboard (copy the seed out, paste a phrase in). arboard's X11 backend
    // serves the selection only while the `Clipboard` object is alive, and
    // dropping it stalls ~2 s trying to hand the contents to a clipboard
    // manager this setup may not have (then the contents are gone). So: create
    // ONE clipboard on first use and deliberately leak it — the X11 selection
    // dies with the process either way.
    let clip: Rc<RefCell<Option<&'static mut arboard::Clipboard>>> = Rc::new(RefCell::new(None));
    fn with_clipboard<R>(
        slot: &Rc<RefCell<Option<&'static mut arboard::Clipboard>>>,
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>,
    ) -> Option<R> {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => *slot = Some(Box::leak(Box::new(cb))),
                Err(e) => {
                    tracing::warn!(error = %e, "clipboard unavailable");
                    return None;
                }
            }
        }
        let cb = slot.as_mut()?;
        match f(cb) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(error = %e, "clipboard access failed");
                None
            }
        }
    }
    {
        let clip = clip.clone();
        ui.on_copy_text(move |text| {
            let _ = with_clipboard(&clip, |cb| cb.set_text(text.to_string()));
        });
    }
    {
        let clip = clip.clone();
        ui.on_paste_clipboard(move || {
            with_clipboard(&clip, arboard::Clipboard::get_text)
                .unwrap_or_default()
                .into()
        });
    }

    // Copy one of the (session-mirrored) run logs as one text block.
    {
        let clip = clip.clone();
        let weak = ui.as_weak();
        ui.on_copy_log(move |which| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let log = match which.as_str() {
                "create" => ui.get_cw_log(),
                "join" => ui.get_jw_log(),
                _ => ui.get_rw_log(),
            };
            let text = log
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let _ = with_clipboard(&clip, |cb| cb.set_text(text));
        });
    }

    // NOTE: the old duplicate-name check is gone by design — display names
    // may repeat, the workspace id disambiguates (the same DAO opened twice
    // locally is a supported setup).

    // The previously applied session settings: the mirror uses it to refresh
    // the settings draft only on real changes, the leave-guard to detect a
    // dirty draft.
    let last_settings: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

    // Chat-bus UI state (selected channel, proposal
    // first-seen times) — UI-local by design, see [`ChatUiState`].
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));

    // what every callback captures: one clone per closure
    let ctx = Ctx {
        rt: rt.clone(),
        wallet: wallet.clone(),
        weak: ui.as_weak(),
        last_settings: last_settings.clone(),
        chat_ui: chat_ui.clone(),
    };

    // The Multisig-Wiki mock's state machine + its WikiState bridge —
    // UI-local by design, EXCEPT the changeset vote: that one proposes on
    // the real gated Memory surface, so it is wired with the handles.
    let (wiki_model, wiki_last) = wire_wiki(&ui);
    wire_wiki_vote(&ui, &ctx, &wiki_model, &wiki_last);
    wire_patch_view(&ui);
    wire_wiki_export(&ui, &ctx);
    wire_wiki_draft(&ui, &ctx);

    // --- actions: each becomes a Command on the shared engine ---
    actions::workspace::wire(&ui, &ctx);
    actions::settings::wire(&ui, &ctx);
    actions::relays::wire(&ui, &ctx);
    actions::ritual::wire(&ui, &ctx);
    actions::chat::wire(&ui, &ctx);
    actions::org::wire(&ui, &ctx);
    // Quit confirmed from the modal: end the Slint event loop so `ui.run()`
    // returns and the process shuts down.
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // --- live-mirror: re-read and re-render on every engine change ---
    {
        let Ctx {
            wallet: w,
            weak,
            last_settings,
            chat_ui,
            ..
        } = ctx.clone();
        rt.spawn(async move {
            let mut rx = w.subscribe();
            push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
            push_surfaces(&w, &weak, &chat_ui).await;
            loop {
                match rx.recv().await {
                    Ok(Event::SessionChanged { scope }) => {
                        push_session(&w, &weak, &last_settings, scope, &chat_ui).await;
                        // A Full session change can mean a workspace was
                        // opened or closed — the surface state (replayed
                        // chat history!) changed with it, without any
                        // chat/proposal event firing. Run-scoped ticks
                        // (90 ms) deliberately skip this.
                        if scope == SessionScope::Full {
                            // a Full change can be a workspace open/close:
                            // proposal ids are per-workspace counters, so a
                            // stale inline-viewer id would light up an id-
                            // colliding card in the NEXT workspace with the
                            // previous one's decoded image — drop it
                            let weak2 = weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak2.upgrade() {
                                    ui.set_img_inline_id(-1);
                                }
                            });
                            push_surfaces(&w, &weak, &chat_ui).await;
                        }
                    }
                    // Any surface event (chat / propose / approve / …) re-reads
                    // the surfaces, so the GUI mirrors what an MCP agent did.
                    // An Event::Chat carries id+channel and could tick unread
                    // counters directly, but the re-read stays the single
                    // source of truth — event payloads never drive state.
                    // A finished download additionally toasts its outcome
                    // (the table repaints via the same re-read).
                    // alert sounds: an INCOMING chat message (never our own
                    // echo) and a new vote play the configured alert — read
                    // from the last APPLIED settings, so an unsaved draft
                    // never changes behavior
                    Ok(Event::Chat { from, .. }) => {
                        alert_unless_own(&last_settings, |s| s.sound_message.clone(), &weak, from);
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // only a vote somebody ELSE initiated rings — the
                    // proposer already knows what they just did
                    Ok(Event::Proposed { by, .. }) => {
                        alert_unless_own(&last_settings, |s| s.sound_vote.clone(), &weak, by);
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // a poke addressed to THIS seat toasts who poked and
                    // rings its own sound (the engine already gated opt-in +
                    // cooldown); the sender side confirms quietly. No
                    // push_surfaces — a poke changes no surface state.
                    Ok(Event::Poked { by, to }) => {
                        alert_unless_own(&last_settings, |s| s.sound_poke.clone(), &weak, by.clone());
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let st = ui.global::<Strings>();
                            let me = ui.get_node_member();
                            if to.as_str() == me.as_str() {
                                ui.invoke_show_toast(format!("{by} {}", st.get_toast_poked()).into());
                            } else if by.as_str() == me.as_str() {
                                ui.invoke_show_toast(
                                    format!("{} {to}", st.get_toast_poke_sent()).into(),
                                );
                            }
                        });
                    }
                    // WP4b: checkpoint lifecycle closure for the operator —
                    // sealed toasts the height, stale tells them to re-cut
                    Ok(Event::CheckpointSealed { height, .. }) => {
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let msg = ui.global::<Strings>().get_toast_checkpoint_sealed();
                            ui.invoke_show_toast(format!("{msg} #{height}").into());
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    // CheckpointStale is NOT toasted: the automation re-cuts
                    // by itself on the very next commit — a "propose again"
                    // instruction would be noise (the event stays on the
                    // stream for MCP observers)
                    Ok(Event::CheckpointStale { .. }) => {
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(Event::FileTransfer { phase, .. }) => {
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(ui) = weak2.upgrade() else { return };
                            let st = ui.global::<Strings>();
                            match &phase {
                                molt_core::TransferPhase::Done { path } => {
                                    ui.invoke_show_toast(
                                        format!("{} {path}", st.get_toast_dl_done()).into(),
                                    );
                                }
                                molt_core::TransferPhase::Failed { reason } => {
                                    ui.invoke_show_toast_error(
                                        format!("{} {reason}", st.get_toast_dl_failed()).into(),
                                    );
                                }
                                _ => {}
                            }
                        });
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Ok(Event::UiActionRequested { action }) => {
                        // gui_over_mcp.md, the drive half: perform the verb
                        // through the SAME callbacks a human's click takes,
                        // then publish — both queue on the event loop, so
                        // the publish claim is post-perform. A pure view
                        // change may not ring the engine, hence the
                        // explicit publish.
                        let weak2 = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak2.upgrade() {
                                perform_ui_action(&ui, &action);
                            }
                        });
                        publish_ui_state(&w, &weak).await;
                    }
                    Ok(_) => push_surfaces(&w, &weak, &chat_ui).await,
                    Err(RecvError::Lagged(_)) => {
                        push_session(&w, &weak, &last_settings, SessionScope::Full, &chat_ui).await;
                        push_surfaces(&w, &weak, &chat_ui).await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    ui.run()
}

/// What every callback captures: the runtime, the shared engine handle,
/// the window, and the two UI-local states the mirror task shares with the
/// callbacks. One clone per closure; the methods are the three ways a
/// click reaches the engine plus the surfaces re-read a view-local change
/// needs (sort, filter, page, channel - state the engine never sees).
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) rt: Handle,
    pub(crate) wallet: WalletHandle,
    pub(crate) weak: slint::Weak<AppWindow>,
    pub(crate) last_settings: Arc<Mutex<Option<SessionSettings>>>,
    pub(crate) chat_ui: Arc<Mutex<ChatUiState>>,
}

impl Ctx {
    /// Fire a command; an engine error surfaces as a toast ([`issue`]).
    pub(crate) fn issue(&self, cmd: Command) {
        issue(&self.rt, &self.wallet, &self.weak, cmd);
    }

    /// [`issue_then_toast`]: the success toast fires only on success.
    pub(crate) fn issue_then_toast(&self, cmd: Command, toast: String) {
        issue_then_toast(&self.rt, &self.wallet, &self.weak, cmd, toast);
    }

    /// [`issue_draft`]: the settings draft through its three doors.
    pub(crate) fn issue_draft(&self, wake: String, settings: SessionSettings) {
        issue_draft(&self.rt, &self.wallet, &self.weak, wake, settings);
    }

    /// Re-read and re-push the surfaces after a UI-local change the engine
    /// does not announce (a sort, a filter, a page, the channel selection).
    pub(crate) fn refresh_surfaces(&self) {
        let cx = self.clone();
        self.rt.spawn(async move {
            push_surfaces(&cx.wallet, &cx.weak, &cx.chat_ui).await;
        });
    }
}

/// [`issue`], plus a success toast that only fires if the command SUCCEEDED.
///
/// The click sites used to toast on the way in: "Proposed" appeared the
/// moment the button was pressed, and a command that then failed showed the
/// user success followed by an error, for a proposal that never existed.
/// The confirmation belongs to the outcome, not to the intent.
fn issue_then_toast(
    rt: &Handle,
    wallet: &WalletHandle,
    weak: &slint::Weak<AppWindow>,
    cmd: Command,
    toast: String,
) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
        let outcome = w.execute(cmd).await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            match outcome {
                Ok(_) => ui.invoke_show_toast(toast.into()),
                Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
            }
        });
    });
}

/// Fire a command on the shared handle; the resulting event drives the
/// live-mirror, so callers do not await a reply — but an engine error is
/// surfaced as a toast instead of vanishing silently.
fn issue(rt: &Handle, wallet: &WalletHandle, weak: &slint::Weak<AppWindow>, cmd: Command) {
    let w = wallet.clone();
    let weak = weak.clone();
    rt.spawn(async move {
        if let Err(e) = w.execute(cmd).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_show_toast_error(error_toast(&ui, &e));
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    /// E5 coverage: the German log table covers EXACTLY the engine's
    /// shape inventory (set-equal both ways); every rendering keeps the
    /// tone glyph and the slot count, a synthesized line round-trips
    /// with its slots intact, and unknown lines / non-German languages
    /// pass through verbatim.
    #[test]
    fn every_log_shape_has_a_german_rendering() {
        use std::collections::BTreeSet;
        let engine: BTreeSet<Vec<&str>> = molt_engine::known_log_shapes()
            .iter()
            .map(|s| s.to_vec())
            .collect();
        let gui: BTreeSet<Vec<&str>> = super::LOG_SHAPES_DE
            .iter()
            .map(|(en, _)| en.to_vec())
            .collect();
        assert_eq!(engine, gui, "engine shapes and the German table diverge");
        for (en, de) in super::LOG_SHAPES_DE {
            assert_eq!(en.len(), de.len(), "slot count differs: {en:?}");
            assert_eq!(
                en[0].chars().next(),
                de[0].chars().next(),
                "tone glyph lost: {en:?}"
            );
            let mut line = String::new();
            let mut want = String::new();
            for (i, (e, d)) in en.iter().zip(de.iter()).enumerate() {
                line.push_str(e);
                want.push_str(d);
                if i + 1 < en.len() {
                    let slot = format!("S{i}");
                    line.push_str(&slot);
                    want.push_str(&slot);
                }
            }
            assert_eq!(
                super::localize_log_line(1, &line),
                want,
                "round-trip failed for {en:?}"
            );
            assert_ne!(want, line, "German rendering equals English: {en:?}");
            assert_eq!(super::localize_log_line(0, &line), line);
        }
        assert_eq!(
            super::localize_log_line(1, "→ some brand new line"),
            "→ some brand new line"
        );
    }

    /// E6: the transport-pill reason, S3 verdicts, Tor details and the
    /// recovery status lines render German part-wise; machine states and
    /// free-text error tails ride verbatim.
    #[test]
    fn e6_maps_render_german_and_keep_tails() {
        use super::{
            localize_net_reason, localize_recover_failed, localize_recover_note,
            localize_s3_verdict, localize_tor_detail, tor_gap_de,
        };
        // net reason: compound parts — member, count and free tail survive
        let r = "link to walter: connecting; sends to mara: io: broken pipe; \
                 relays: no relay accepted the subscription; 3 frames past the key ring";
        assert_eq!(
            localize_net_reason(1, r),
            "Verbindung zu walter: verbinde; Zustellung an mara: io: broken pipe; \
             Relays: kein Relay nahm die Subscription an; 3 Frames jenseits des Schlüsselrings"
        );
        assert_eq!(localize_net_reason(0, r), r);
        assert_eq!(
            localize_net_reason(1, "no live relay connection (0 of 3 up, reconnecting)"),
            "keine lebende Relay-Verbindung (0 von 3 erreichbar, verbinde neu)"
        );
        // the offline statics match by prefix (the engine wraps their tails)
        assert!(localize_net_reason(
            1,
            "offline: no mesh links on disk - rejoin via a recovery link"
        )
        .starts_with("offline: keine Mesh-Links"));
        // s3: machine states untouched; shells + hints localized, code rides
        assert_eq!(localize_s3_verdict(1, "testing"), "testing");
        assert_eq!(localize_s3_verdict(1, "ok"), "ok");
        assert_eq!(
            localize_s3_verdict(1, "error: endpoint: no bucket configured"),
            "Fehler: Endpunkt: kein Bucket konfiguriert"
        );
        assert_eq!(
            localize_s3_verdict(
                1,
                "error: http 403: access denied - check access key and secret (AccessDenied)"
            ),
            "Fehler: HTTP 403: Zugriff verweigert - Access-Key und Secret prüfen (AccessDenied)"
        );
        assert_eq!(
            localize_s3_verdict(1, "error: http 404: bucket `media` not found"),
            "Fehler: HTTP 404: Bucket `media` nicht gefunden"
        );
        // tor: the four gap clauses stay distinct; rung tails verbatim
        let gaps = [
            "no relay is configured",
            "no relay is confirmed yet",
            "the confirmed relays need non-onion dialing, which is switched off",
            "only local relays are configured, and those bypass Tor",
        ];
        let mut des: Vec<String> = gaps.iter().map(|g| tor_gap_de(g)).collect();
        for (g, d) in gaps.iter().zip(&des) {
            assert_ne!(d, g, "gap clause without a German arm: {g}");
        }
        des.sort();
        des.dedup();
        assert_eq!(des.len(), 4, "gap renderings collide");
        assert_eq!(
            localize_tor_detail(1, "no circuit was proven - no relay is confirmed yet"),
            "kein Circuit bewiesen - noch kein Relay bestätigt"
        );
        assert_eq!(
            localize_tor_detail(1, "no relay handshake through Tor to x.onion: timed out"),
            "no relay handshake through Tor to x.onion: timed out"
        );
        // recovery: known notes + failure prefixes, tails verbatim
        assert_eq!(
            localize_recover_note(1, "waiting for the coordinator's Welcome (7 min)"),
            "warte auf das Welcome des Koordinators (7 min)"
        );
        assert_eq!(
            localize_recover_failed(1, "recovery request: relay refused"),
            "Recovery-Anfrage: relay refused"
        );
    }

    /// E6: every wiki-side refusal literal renders German — pinned against
    /// the SOURCE, so a new `Err("…")` in wiki.rs goes red here until it
    /// gets an arm in `localize_wiki_err`.
    #[test]
    fn every_wiki_error_renders_german() {
        let src = include_str!("wiki.rs");
        let mut found = 0;
        for part in src.split("Err(\"").skip(1) {
            let lit = part.split('"').next().expect("literal terminates");
            found += 1;
            let de = super::localize_wiki_err(1, lit);
            assert_ne!(de, lit, "wiki error without a German arm: {lit:?}");
            assert!(!de.is_empty());
        }
        assert!(found >= 20, "the wiki.rs error scan found only {found} sites");
        // honest fallback + non-German identity
        assert_eq!(super::localize_wiki_err(1, "some new error"), "some new error");
        assert_eq!(super::localize_wiki_err(0, "unknown folder"), "unknown folder");
    }

    /// E3 coverage: every headline phrase the engine can emit has a
    /// German rendering — a new phrase without one goes red here instead
    /// of silently showing English in the German UI. (The engine pins the
    /// inventory producible; this pins it translated.)
    #[test]
    fn every_engine_headline_has_a_german_rendering() {
        for phrase in molt_engine::known_headlines() {
            let de = super::localize_headline(1, phrase);
            assert_ne!(
                &de, phrase,
                "phrase without a German arm: {phrase}"
            );
            assert!(!de.is_empty());
        }
        // …and the honest fallback: unknown phrases render as themselves
        assert_eq!(super::localize_headline(1, "Brand new phrase"), "Brand new phrase");
        assert_eq!(super::localize_headline(0, "No shared relay"), "No shared relay");
    }

    /// E2: the error toast renders in the active language, and the match
    /// carries NO wildcard — a new MoltError variant fails compilation in
    /// `localize_error` until it gets a German arm.
    #[test]
    fn engine_errors_render_in_the_active_language() {
        let e = molt_core::MoltError::UnknownProposal(molt_core::ProposalId(7));
        assert_eq!(super::localize_error(0, &e), e.to_string(), "EN = engine Display (MCP parity)");
        assert_eq!(super::localize_error(1, &e), "Unbekannter Vorschlag #7");
        let e = molt_core::MoltError::WorkspaceEncrypted("R".to_string());
        assert!(super::localize_error(1, &e).contains("versiegelt"));
    }

    /// R1 (relay_topology_plan): the create wizard states rule 1 — ONE
    /// relay every member can reach (the join runs over the INTERSECTION;
    /// "identical pool" was a stricter, false rule that contradicted the
    /// engine's own gate) — plus the self-hosted branch.
    #[test]
    fn the_create_wizard_states_the_one_shared_relay_rule() {
        for l in [Lexicon::en(), Lexicon::de()] {
            let h = l.cw_relays_hint;
            assert!(
                h.contains("ONE relay") || h.contains("EIN Relay"),
                "branch 1 - one shared relay: {h}"
            );
            assert!(
                h.to_lowercase().contains("pool"),
                "branch 2 - the self-hosted relay in every pool: {h}"
            );
            assert!(
                !h.contains("identical") && !h.contains("identischen"),
                "the pool need not be identical - the join runs over the intersection: {h}"
            );
        }
    }

    /// L10: the retention pair renders its unit in the ACTIVE language —
    /// the payload carries the machine value, and a legacy "30 days"
    /// normalizes by its leading number instead of leaking English into
    /// the German card.
    #[test]
    fn the_retention_pair_renders_its_unit_in_the_active_language() {
        assert_eq!(super::retention_value(0, "7"), "7 days");
        assert_eq!(super::retention_value(1, "7"), "7 Tage");
        assert_eq!(super::retention_value(1, "30 days"), "30 Tage");
        assert_eq!(super::retention_value(0, ""), "", "unknown stays untouched");
    }

    use super::*;
    use std::collections::HashMap;

    use molt_core::relay::{RelayBlock, RelayKind, RelayStatus};
    use molt_core::{
        ChannelInfo, ChannelRef, ChatMessage, MessageId, ProposalId, ProposalState, ProposalView,
        SessionView, Surface,
    };

    /// The set_relays vote card shows the CHANGES: every pool member of the
    /// union, marked kept / added / removed, in current-then-added order.
    /// Review 2026-08-12: a set_features card must never paint a red
    /// "removed" row - the union fold cannot remove, and `current` is
    /// recomputed live, so a racing enable would otherwise show an
    /// impossible removal on a governance card. Keys render as display
    /// labels (one vocabulary with nav and wizard).
    #[test]
    fn a_feature_diff_never_shows_a_removal_and_renders_labels() {
        let pv = ProposalView {
            id: ProposalId(7),
            surface: Surface::Organization,
            payload: serde_json::json!({ "op": "set_features", "value": "memory quests" }),
            approvals: 1,
            threshold: 2,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            // a racing enable made "vault" effective AFTER this was proposed
            current: "memory vault".to_string(),
            proposed: "memory quests".to_string(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let row = proposal_row(0, &pv);
        assert!(
            row.relay_changes.iter().all(|(sign, _)| *sign != RELAY_ROW_REMOVED),
            "a feature diff row claimed a removal: {:?}",
            row.relay_changes
        );
        assert!(
            row.relay_changes
                .iter()
                .any(|(sign, label)| *sign == RELAY_ROW_KEPT && label == "Vault"),
            "the racing enable renders as kept, labelled: {:?}",
            row.relay_changes
        );
        assert!(
            row.relay_changes
                .iter()
                .any(|(sign, label)| *sign == RELAY_ROW_ADDED && label == "Kanban"),
            "the addition renders with its display label: {:?}",
            row.relay_changes
        );
    }

    #[test]
    fn relay_pool_diff_marks_added_removed_kept() {
        let rows = relay_pool_diff("wss://a wss://b", "wss://b wss://c");
        assert_eq!(
            rows,
            vec![
                (RELAY_ROW_REMOVED, "wss://a".to_string()),
                (RELAY_ROW_KEPT, "wss://b".to_string()),
                (RELAY_ROW_ADDED, "wss://c".to_string()),
            ]
        );
        // identical pools: everything kept, nothing invented
        assert_eq!(
            relay_pool_diff("wss://a", "wss://a"),
            vec![(RELAY_ROW_KEPT, "wss://a".to_string())]
        );
        // duplicates in a hand-written proposal collapse
        assert_eq!(
            relay_pool_diff("", "wss://x wss://x"),
            vec![(RELAY_ROW_ADDED, "wss://x".to_string())]
        );
        // an empty proposed pool folds as a no-op engine-side, so the card
        // must NOT promise removals — no rows, generic fallback
        assert_eq!(relay_pool_diff("wss://a", ""), Vec::<(i32, String)>::new());
    }

    fn line(lead: &str, text: &str) -> LogLineData {
        LogLineData {
            id: String::new(),
            lead: lead.to_string(),
            text: text.to_string(),
            when: String::new(),
            quote: -1,
            quote_id: String::new(),
            system: false,
            quote_label: String::new(),
            quote_indent: 0,
            deleted_by: String::new(),
            first: true,
            own: false,
            alt: false,
            mine_emoji: String::new(),
            reactions: Vec::new(),
            receipts: Vec::new(),
            has_file: false,
            file_name: String::new(),
            file_meta: String::new(),
            file_available: false,
            proposal_id: None,
        }
    }

    /// A deterministic 32-char hex id for tests.
    fn hex_id(b: u8) -> String {
        MessageId([b; 16]).to_string()
    }

    fn qsrc(lead: &str, text: &str, deleted: bool) -> QuoteSrc {
        QuoteSrc {
            lead: lead.to_string(),
            text: text.to_string(),
            deleted,
        }
    }

    /// The pending-card image preview decodes the payload bytes that rode
    /// the `set_image` proposal — for EVERY format the propose-side picker
    /// offers (png, jpg, jpeg, webp, gif, svg, bmp). The decode must key on
    /// the CONTENT, never on a file extension: the payload is raw bytes, no
    /// name travels with it. (This pins the bug where the bytes were staged
    /// as a `.img` temp file and `slint::Image::load_from_path` — which
    /// trusts extensions — failed for every proposal, so "Click to view the
    /// proposed image" only ever produced the failure toast.)
    #[test]
    fn a_proposed_image_decodes_from_the_payload_for_every_picker_format() {
        // real minimal files, one per picker format (2x2 red, PIL-generated)
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGM8ISfHwMDAxMDAwMDAAAANBAEIfXHKZgAAAABJRU5ErkJggg==";
        let gif = "R0lGODdhAgACAIEAAMgeHgAAAAAAAAAAACwAAAAAAgACAAAIBgABCAQQEAA7";
        let bmp = "Qk1GAAAAAAAAADYAAAAoAAAAAgAAAAIAAAABABgAAAAAABAAAADEDgAAxA4AAAAAAAAAAAAAHh7IHh7IAAAeHsgeHsgAAA==";
        let webp = "UklGRjoAAABXRUJQVlA4IC4AAACwAQCdASoCAAIAAUAmJaACdLoABDAAAP7x3I/4DdfFtMv/vYL/3YL/3YL/WwAA";
        let jpeg = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjL/wAARCAACAAIDASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwDkKKKK8U/TD//Z";
        for (fmt, b64) in [
            ("png", png),
            ("gif", gif),
            ("bmp", bmp),
            ("webp", webp),
            ("jpeg", jpeg),
        ] {
            let img = proposal_image_from_b64(b64);
            assert!(img.is_some(), "the {fmt} payload must decode");
            let img = img.expect("checked above");
            assert_eq!(img.size().width, 2, "{fmt} decodes to the real picture");
            assert_eq!(img.size().height, 2, "{fmt} decodes to the real picture");
        }
        // svg travels as its source text
        use base64::Engine as _;
        let svg = base64::engine::general_purpose::STANDARD.encode(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#f00"/></svg>"##,
        );
        assert!(
            proposal_image_from_b64(&svg).is_some(),
            "an svg payload must decode"
        );
    }

    /// Undecodable payloads answer `None` — the caller shows the honest
    /// "could not be decoded" toast, never a broken image.
    #[test]
    fn an_undecodable_image_payload_is_none_not_a_panic() {
        assert!(proposal_image_from_b64("").is_none(), "empty payload");
        assert!(
            proposal_image_from_b64("not base64 at all!").is_none(),
            "not base64"
        );
        use base64::Engine as _;
        let garbage = base64::engine::general_purpose::STANDARD.encode([0x00u8; 64]);
        assert!(
            proposal_image_from_b64(&garbage).is_none(),
            "valid base64, but not an image"
        );
    }

    /// **The `Strings`/`lexicon!` pairing is guarded in ONE direction
    /// only**: an entry whose field has no property fails to compile, but
    /// a property with no entry compiles and renders as an EMPTY string in
    /// both languages. This scans the two sources against each other, so a
    /// forgotten pair goes red here instead of shipping a blank label.
    #[test]
    fn every_strings_property_has_an_english_and_a_german_arm() {
        let theme = include_str!("../../molt-ui-window/ui/theme.slint");
        let lex = include_str!("i18n.rs");
        // the Strings global alone - Theme, HintTip and Poke declare
        // string properties of their own
        let block = theme
            .split("export global Strings {")
            .nth(1)
            .expect("the Strings global")
            .split("\n}")
            .next()
            .expect("the global closes");
        let mut keys = 0;
        for line in block.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.split_once("property <string> ") else {
                continue;
            };
            let key = rest
                .1
                .split([';', ':'])
                .next()
                .expect("a property name")
                .trim();
            keys += 1;
            let field = key.replace('-', "_");
            assert!(
                lex.contains(&format!("\n    {field}: \"")),
                "Strings.{key} has no lexicon! entry - it renders EMPTY"
            );
        }
        assert!(keys > 500, "the Strings scan found only {keys} properties");
    }

    // ---------------------------------------------------------------
    // Member profiles (`member_profiles_plan.md` §5): the picture a seat
    // proposes for itself is fitted HERE - square and inside this
    // republic's served budget - before the engine ever sees it.
    // ---------------------------------------------------------------

    /// A `w x h` picture with incompressible content: a flat colour would
    /// fit any budget at any edge and prove nothing about the downscale.
    pub(super) fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        let mut seed: u32 = 0x1234_5678;
        for p in img.pixels_mut() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *p = image::Rgb([(seed >> 16) as u8, (seed >> 8) as u8, seed as u8]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    /// The engine refuses a non-square member picture (every frontend
    /// renders it in a square box), so the fit crops from the CENTRE -
    /// a top-left crop would behead every portrait.
    #[test]
    fn a_wide_picture_is_center_cropped_to_a_square() {
        use image::GenericImageView as _;
        let wide = noisy_png(40, 20);
        let fitted = fit_member_image(&wide, 1 << 20).expect("a small picture fits");
        let out = image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
        assert_eq!(
            out.width(),
            out.height(),
            "the engine refuses a non-square picture"
        );
        assert_eq!(out.width(), 20, "the square is the shorter edge");
        let src = image::load_from_memory(&wide).expect("source decodes");
        assert_eq!(
            out.get_pixel(0, 0),
            src.get_pixel(10, 0),
            "the crop starts at the middle, not at the left edge"
        );
    }

    /// The served budget is the promise the engine keeps; a picture over
    /// it is stepped down until it fits, not sent to be refused.
    #[test]
    fn an_oversized_picture_lands_inside_the_budget() {
        let big = noisy_png(1024, 1024);
        let budget = 40 * 1024;
        assert!(big.len() > budget, "the fixture must actually be oversized");
        let fitted = fit_member_image(&big, budget).expect("a downscale fits it");
        assert!(
            fitted.bytes.len() <= budget,
            "{} bytes over a {budget} byte budget",
            fitted.bytes.len()
        );
        image::load_from_memory(&fitted.bytes).expect("the fit stays a picture");
    }

    /// A picture that is already square and already small travels as the
    /// bytes the user picked - a re-encode would only lose quality.
    #[test]
    fn a_picture_that_already_fits_is_proposed_untouched() {
        let small = noisy_png(64, 64);
        let fitted = fit_member_image(&small, 1 << 20).expect("it fits");
        assert_eq!(fitted.bytes, small, "no re-encode when none is needed");
        assert_eq!(fitted.ext, "png", "the name must not lie about the format");
    }

    /// Below the floor the honest answer is a refusal: a 128px avatar that
    /// still does not fit means the republic has no room for a picture.
    #[test]
    fn a_budget_below_the_floor_is_refused_honestly() {
        let big = noisy_png(1024, 1024);
        assert!(
            matches!(fit_member_image(&big, 400), Err(ImageFitError::TooLarge)),
            "an unreachable budget must refuse, never ship a 1px avatar"
        );
    }

    /// Undecodable bytes are caught by the frontend's real decoder, the
    /// same pre-check `on_org_propose` runs for the logo.
    #[test]
    fn undecodable_bytes_never_reach_the_proposal() {
        assert!(matches!(
            fit_member_image(b"not an image at all", 1 << 20),
            Err(ImageFitError::Undecodable)
        ));
    }

    /// A seat that REPLACES its picture keeps the same file name
    /// (`avatar-<stem>.<ext>`), so a path-only cache key would keep
    /// showing the old face until the app restarts. The key carries the
    /// file's identity, not just its name.
    /// The republic's picture must survive a REPLACEMENT: same file name,
    /// new content. A path compare says "unchanged" and the window keeps the
    /// old logo until a restart - the bug this rule replaced.
    #[test]
    fn a_replaced_logo_forces_a_reload_although_its_path_is_unchanged() {
        let tmp = tempfile::tempdir().expect("tmp");
        let logo = tmp.path().join("logo.png");
        let path = logo.display().to_string();
        std::fs::write(&logo, noisy_png(8, 8)).expect("write the first logo");

        let first = super::logo_needs_reload("", &path).expect("a first picture always loads");
        assert_eq!(
            super::logo_needs_reload(&first, &path),
            None,
            "an unchanged picture must not be decoded again on every push"
        );

        std::fs::write(&logo, noisy_png(16, 16)).expect("replace the logo");
        let second = super::logo_needs_reload(&first, &path)
            .expect("a replaced picture must reload behind its unchanged path");
        assert_ne!(first, second, "the key moves with the content");

        assert_eq!(
            super::logo_needs_reload("", ""),
            None,
            "a republic without a picture never reloads one"
        );
    }

    #[test]
    fn the_avatar_cache_key_moves_when_the_file_content_does() {
        let tmp = tempfile::tempdir().expect("tmp");
        let path = tmp.path().join("avatar-walter.png");
        std::fs::write(&path, noisy_png(8, 8)).expect("write");
        let p = path.display().to_string();
        let first = avatar_cache_key(&p);
        assert!(first.starts_with(&p), "the key still names the file: {first}");
        assert_eq!(first, avatar_cache_key(&p), "an untouched file keys the same");
        // the same NAME, a different picture
        std::fs::write(&path, noisy_png(16, 16)).expect("rewrite");
        assert_ne!(
            first,
            avatar_cache_key(&p),
            "a replaced picture must invalidate the cached decode"
        );
        assert_eq!(avatar_cache_key(""), "", "no picture, no key");
    }

    /// `sync_rows` rewrites EVERY row on EVERY mirror push, so a decode
    /// inside the row mapping would re-decode the whole roster per tick.
    #[test]
    fn an_avatar_decodes_once_per_path_and_forgets_the_gone_ones() {
        let mut cache = AvatarCache::default();
        let loads = std::cell::Cell::new(0);
        let load = |_p: &str| {
            loads.set(loads.get() + 1);
            Some(slint::Image::default())
        };
        assert!(cache.get("/w/avatar-a.png", load).is_some());
        assert!(cache.get("/w/avatar-a.png", load).is_some());
        assert_eq!(loads.get(), 1, "one decode per path, not per push");
        // a miss is remembered too - a picture whose file is not on this
        // device must not re-stat on every tick either
        let missing = |_p: &str| {
            loads.set(loads.get() + 1);
            None
        };
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert_eq!(loads.get(), 2, "the miss is cached like the hit");
        let live: std::collections::HashSet<&str> = ["/w/avatar-a.png"].into_iter().collect();
        cache.retain_live(&live);
        assert!(cache.get("/w/gone.png", missing).is_none());
        assert_eq!(loads.get(), 3, "a dropped path decodes again");
    }

    /// One `ProposalView` carrying a member-profile payload.
    fn profile_view(op: &str, member: &str) -> ProposalView {
        let mut v = view_of(1, "", ProposalState::Proposed);
        v.surface = Surface::Organization;
        v.payload = serde_json::json!({ "op": op, "member": member });
        v
    }

    /// A member picture rides the same inline-preview and save path the
    /// org logo has - the bytes are in the payload either way.
    #[test]
    fn a_member_picture_proposal_offers_the_preview() {
        for op in ["set_member_image", "remove_member_image"] {
            assert!(
                proposal_row(0, &profile_view(op, "walter")).image_op,
                "{op} must render as a picture change"
            );
        }
        assert!(
            !proposal_row(0, &profile_view("set_member_desc", "walter")).image_op,
            "a description carries no picture"
        );
        let mut v = profile_view("set_member_image", "walter");
        v.payload["bytes_b64"] = serde_json::json!("QUJD");
        assert_eq!(
            proposal_row(0, &v).img_b64,
            "QUJD",
            "the bytes reach the preview"
        );
    }

    /// A profile change is about ONE seat - the card says whose.
    #[test]
    fn member_profile_titles_name_the_seat_in_both_languages() {
        for (op, en, de) in [
            ("set_member_image", "Picture: walter", "Bild: walter"),
            (
                "set_member_desc",
                "Description: walter",
                "Beschreibung: walter",
            ),
            (
                "remove_member_image",
                "Remove picture: walter",
                "Bild entfernen: walter",
            ),
        ] {
            let payload = serde_json::json!({ "op": op, "member": "walter" });
            assert_eq!(display_title(0, &payload), en);
            assert_eq!(display_title(1, &payload), de);
        }
        // a profile payload without a seat cannot claim one
        let anon = serde_json::json!({ "op": "set_member_desc", "value": "hi" });
        assert!(!display_title(0, &anon).contains("Description:"));
    }

    /// An engine-authored System-kind message maps onto the same per-line
    /// `system` flag the governance rows use — one quiet rendering path,
    /// never a second style; a User message stays a normal card.
    #[test]
    fn a_system_kind_message_maps_onto_the_quiet_line_flag() {
        let user = ChatMessage::text(MessageId([1; 16]), "petra", "gm", 100);
        assert!(!chat_line(0, &user, "me", &[]).system);
        let notice = ChatMessage::text(MessageId([2; 16]), "petra", "🔑 back", 101)
            .with_kind(molt_core::ChatKind::System);
        assert!(chat_line(0, &notice, "me", &[]).system);
    }

    /// Read receipts show ONLY on the local member's own messages (the sender
    /// wants delivery confirmation) — one dot per OTHER member, green once in
    /// read_by; an incoming message carries no receipt row at all.
    #[test]
    fn read_receipts_render_only_on_own_messages() {
        let roster = vec!["me".to_string(), "ada".to_string(), "bo".to_string()];

        // my own message: a dot per OTHER member, ada green (read), bo yellow
        let mut mine = ChatMessage::text(MessageId([3; 16]), "me", "hi", 100);
        mine.read_by.insert("ada".to_string());
        let r = chat_line(0, &mine, "me", &roster).receipts;
        assert_eq!(r.len(), 2, "one dot per other member");
        assert_eq!(r.iter().find(|x| x.name == "ada").map(|x| x.read), Some(true));
        assert_eq!(r.iter().find(|x| x.name == "bo").map(|x| x.read), Some(false));
        assert!(r.iter().all(|x| x.name != "me"), "the author gets no self-dot");

        // an incoming message (not mine): NO receipt row
        let mut theirs = ChatMessage::text(MessageId([4; 16]), "ada", "yo", 101);
        theirs.read_by.insert("me".to_string());
        assert!(
            chat_line(0, &theirs, "me", &roster).receipts.is_empty(),
            "a received message shows no receipts"
        );
    }

    /// The recovery flow rides the transient session notice (the engine's
    /// contract: `recovery-link-pending:` / `recovery-link:` /
    /// `recovery-link-failed:` / `recover-started:` / `recover-failed:` /
    /// `recovered:`); the parser must split each prefix off verbatim and
    /// treat everything else — including the existing notices — as none.
    #[test]
    fn recover_notices_parse_into_their_ui_effects() {
        assert_eq!(
            parse_recover_notice("recovery-link:molt://recover/abc"),
            RecoverNotice::Link("molt://recover/abc".to_string())
        );
        assert_eq!(
            parse_recover_notice("recover-started:ashi"),
            RecoverNotice::Started("ashi".to_string())
        );
        assert_eq!(
            parse_recover_notice("recover-failed:the survivors declined"),
            RecoverNotice::Failed("the survivors declined".to_string())
        );
        assert_eq!(
            parse_recover_notice("recovered:ashi"),
            RecoverNotice::Done("ashi".to_string())
        );
        // the coordinator's mint lifecycle: pending on the attempt, then the
        // outcome — a calm failed state (the flip side of Link) whose payload
        // is a reason the dialog maps onto localized text
        assert_eq!(
            parse_recover_notice("recovery-link-pending:ashi"),
            RecoverNotice::LinkPending("ashi".to_string())
        );
        assert_eq!(
            parse_recover_notice("recovery-link-failed:mesh-not-running"),
            RecoverNotice::LinkFailed("mesh-not-running".to_string())
        );
        // `recovery-link-failed:` must not be swallowed by the shorter
        // `recovery-link:` prefix — order in the parser matters
        assert_eq!(
            parse_recover_notice("recovery-link-failed:transport: queue gone"),
            RecoverNotice::LinkFailed("transport: queue gone".to_string())
        );
        // the non-recovery notices stay untouched by this path
        assert_eq!(parse_recover_notice("saved"), RecoverNotice::None);
        assert_eq!(parse_recover_notice("save-failed: disk"), RecoverNotice::None);
        assert_eq!(parse_recover_notice(""), RecoverNotice::None);
        // an error that itself contains a colon survives whole
        assert_eq!(
            parse_recover_notice("recover-failed:transport: queue gone"),
            RecoverNotice::Failed("transport: queue gone".to_string())
        );
    }

    /// Rewrite of the pre-chat-bus author-block/teaser tests, meaning
    /// preserved: header once per block, zebra flips on author change,
    /// quotes tease "author: body", dangling quotes are dropped — but the
    /// quotes are now id-addressed, resolve their teaser through the
    /// full-log map (so a cross-channel quote teases without a jump row)
    /// and deleted targets tease with an ellipsis.
    #[test]
    fn annotate_chat_log_resolves_quotes_by_id() {
        let mut log = vec![
            line("me", "first"),
            line("me", "second"),
            line("ashi", "answer"),
            line("me", "back"),
        ];
        for (i, l) in log.iter_mut().enumerate() {
            l.id = hex_id(u8::try_from(i).expect("tiny") + 1);
        }
        log[2].quote_id = hex_id(1); // in view → teaser + jump row
        log[3].quote_id = hex_id(99); // dangling id → dropped
        let quotes = HashMap::from([(hex_id(1), qsrc("me", "first", false))]);
        annotate_chat_log(&mut log, &quotes);
        // the header shows once per author block …
        assert_eq!(
            log.iter().map(|l| l.first).collect::<Vec<_>>(),
            [true, false, true, true]
        );
        // … and the zebra flips exactly on author changes
        assert_eq!(
            log.iter().map(|l| l.alt).collect::<Vec<_>>(),
            [false, false, true, false]
        );
        assert_eq!(log[2].quote_label, "me: first");
        assert_eq!(log[2].quote, 0, "the jump target is the quoted row");
        assert_eq!(log[3].quote, -1, "dangling quotes are dropped");
        assert_eq!(log[3].quote_label, "");

        // a deleted target teases with an ellipsis; a target OUTSIDE the
        // displayed log (cross-channel quote — the sanctioned cross-post)
        // teases from the full-log map but offers no jump row
        let mut log = vec![line("ashi", "reply")];
        log[0].id = hex_id(2);
        log[0].quote_id = hex_id(1);
        let quotes = HashMap::from([(hex_id(1), qsrc("me", "", true))]);
        annotate_chat_log(&mut log, &quotes);
        assert_eq!(log[0].quote_label, "me: …");
        assert_eq!(log[0].quote, -1, "not in view: teaser without a jump");

        // legacy numeric quotes (pre-chat-bus rows) still resolve by row
        let mut log = vec![line("me", "first"), line("ashi", "answer"), line("me", "back")];
        log[1].quote = 0;
        log[2].quote = 99; // out of range
        annotate_chat_log(&mut log, &HashMap::new());
        assert_eq!(log[1].quote_label, "me: first");
        assert_eq!(log[2].quote, -1, "out-of-range legacy quotes are dropped");
    }

    #[test]
    fn derive_channels_lists_only_open_vote_discussions() {
        let known_of = |title: &str, fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": title}),
            surface: Surface::Memory,
            approvals: 1,
            threshold: 2,
            fate,
        };
        let infos = vec![
            ChannelInfo {
                channel: ChannelRef::Topic { name: "zeta".into() },
                count: 4,
                last_ts: 40,
                state: None,
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(7) },
                count: 1,
                last_ts: 30,
                state: None,
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(5) },
                count: 2,
                last_ts: 20,
                state: Some(ProposalState::Applied),
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Patch { id: ProposalId(3) },
                count: 5,
                last_ts: 10,
                state: Some(ProposalState::Proposed),
                unread: 0,
            },
            ChannelInfo {
                channel: ChannelRef::Group,
                count: 9,
                last_ts: 50,
                state: None,
                unread: 0,
            },
        ];
        let known = HashMap::from([
            (3u64, known_of("raise budget", KnownFate::Pending)),
            (5u64, known_of("sealed one", KnownFate::Applied)),
        ]);
        let unread = HashMap::from([("patch:3".to_string(), 2usize), ("group".to_string(), 1)]);
        let rows = derive_channels(0, &infos, &known, &unread);
        // topics first (a human named them), then the discussions of OPEN
        // votes. No group row - the Gruppe nav view covers it - and no
        // sealed/closed votes or unknown proposals: a discussion is
        // vote-bound and dies with its vote.
        //
        // The TOPIC row is the one this list lost once, and losing it made
        // the New-topic button a trapdoor: the channel existed and held
        // messages with nowhere to click back to.
        assert_eq!(
            rows.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["topic:zeta", "patch:3"],
            "the topic keeps its row; only the open vote's discussion survives"
        );
        assert_eq!(rows[0].label, "zeta", "a topic is labelled by its name");
        assert_eq!(rows[1].label, "raise budget", "patch title from proposal state");
        assert_eq!(rows[1].unread, 2);
        // nothing open → no rows (the sidebar hides the whole section)
        let rows = derive_channels(0, &[], &HashMap::new(), &HashMap::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn vote_jump_targets_the_hosting_surface_and_fate_view() {
        let known_of = |surface: Surface, fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": "t"}),
            surface,
            approvals: 0,
            threshold: 2,
            fate,
        };
        let known = HashMap::from([
            (5u64, known_of(Surface::Organization, KnownFate::Pending)),
            (6u64, known_of(Surface::Organization, KnownFate::Closed)),
            (7u64, known_of(Surface::Memory, KnownFate::Pending)),
        ]);
        // only a patch channel has a vote to jump back to
        assert!(vote_jump_command(&ChannelRef::Group, &known).is_none());
        let topic = ChannelRef::Topic { name: "zeta".to_string() };
        assert!(vote_jump_command(&topic, &known).is_none());
        // an open Organization vote → its card sits in the pending view
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(5) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
        ));
        // a closed one moved to the declined view
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(6) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "declined"
        ));
        // a gated surface hosts its cards on its main view — plain surface
        // selection, exactly like the sidebar row
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(7) }, &known),
            Some(Command::SelectSurface { surface: Surface::Memory })
        ));
        // a cache miss (this UI never saw the proposal) falls back to the
        // Organization pending view — never a dead button
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(99) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "pending"
        ));
        // WP1: an APPLIED Organization vote's row lives in the accepted view
        let known = HashMap::from([(8u64, {
            let mut k = known_of(Surface::Organization, KnownFate::Applied);
            k.approvals = 2;
            k
        })]);
        assert!(matches!(
            vote_jump_command(&ChannelRef::Patch { id: ProposalId(8) }, &known),
            Some(Command::SelectView { surface: Surface::Organization, view }) if view == "accepted"
        ));
    }

    /// Discussion/card titles must never mix languages: an org governance
    /// payload carries the machine `op` as its placeholder and the UI
    /// translates it AT RENDER TIME in the active language — never a
    /// pre-rendered string frozen in whatever language the proposer's UI
    /// happened to be in. User content (note titles) passes through.
    #[test]
    fn org_titles_render_in_the_active_language_from_the_op_placeholder() {
        let payload = serde_json::json!({"op": "set_name", "value": "Neu"});
        assert_eq!(display_title(0, &payload), "Rename");
        assert_eq!(display_title(1, &payload), "Name ändern");
        // a legacy payload with a baked, possibly foreign-language title:
        // the op placeholder still wins for governance ops
        let legacy =
            serde_json::json!({"op": "set_image", "title": "Logo ändern", "value": "x.png"});
        // short noun labels: the sidebar channel list elides long titles,
        // and a leading "Change …" verb is redundant on a proposal anyway
        assert_eq!(display_title(0, &legacy), "Logo");
        // user content is the title — untouched, in any language
        let note = serde_json::json!({"op": "add_note", "title": "budget"});
        assert_eq!(display_title(0, &note), "budget");
        assert_eq!(display_title(1, &note), "budget");
    }

    /// WP1: an applied log line carries the id of the proposal that produced
    /// it (the snapshot's parallel id track), so the row can offer the 💬
    /// jump into the vote's discussion. A row with no known origin (legacy
    /// dump, pre-id peer) carries none and must offer no jump.
    #[test]
    fn applied_log_lines_carry_their_patch_id() {
        let snap = molt_core::SurfaceSnapshot {
            surface: Surface::Memory,
            gated: true,
            applied: vec![
                serde_json::json!({"op": "add_note", "title": "a"}),
                serde_json::json!({"op": "add_note", "title": "b"}),
            ],
            applied_ids: vec![Some(7), None],
            pending: Vec::new(),
            denied: 0,
            declined: Vec::new(),
            accepted: vec![ProposalView {
                id: ProposalId(7),
                surface: Surface::Memory,
                payload: serde_json::json!({"op": "add_note", "title": "a"}),
                approvals: 2,
                threshold: 2,
                state: molt_core::ProposalState::Applied,
                approved_by_me: true,
                declined_by_me: false,
                current: String::new(),
                proposed: String::new(),
                votes: vec![
                    molt_core::MemberVote {
                        member: "petra".to_string(),
                        vote: molt_core::VoteState::Approved,
                    },
                    molt_core::MemberVote {
                        member: "walter".to_string(),
                        vote: molt_core::VoteState::Approved,
                    },
                ],
                declined_at: 0,
                declined_by: String::new(),
                by: String::new(),
                mine: false,
                superseded: false,
                withdrawn: false,
            }],
            channels: Vec::new(),
            has_archive: false,
            wiki_tree: Vec::new(),
            wiki_rev: 0,
        };
        let data = surface_data(0, Surface::Memory, &snap, "petra", None, &HashMap::new());
        assert_eq!(data.log.len(), 2);
        assert_eq!(data.log[0].proposal_id, Some(7));
        assert_eq!(data.log[1].proposal_id, None);
        // the Accepted table: newest first, the proposal-backed row carries
        // its voters, the legacy row (unknown origin) only its title
        assert_eq!(data.accepted.len(), 2);
        assert_eq!(data.accepted[0].id, -1, "legacy row, no discussion jump");
        assert_eq!(data.accepted[1].id, 7);
        assert_eq!(data.accepted[1].votes.len(), 2, "the block-proven voters");
    }

    #[test]
    fn system_lines_interleave_by_time_and_tolerate_unknown_proposals() {
        let pv = ProposalView {
            id: ProposalId(4),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
            approvals: 2,
            threshold: 3,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(0, 4, &[pv], &HashMap::new(), &first_seen);
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0].0, 150, "stamped with the UI-side first-seen time");
        assert!(sys[0].1.system, "system lines carry the quiet-style flag");
        assert!(sys[0].1.lead.is_empty(), "system lines have no author");
        assert!(sys[0].1.id.is_empty(), "no id → no id-requiring actions");
        let text = &sys[0].1.text;
        assert!(
            text.contains("#4") && text.contains("budget") && text.contains("2/3"),
            "{text}"
        );

        // an unknown/already-materialized proposal renders as a bare
        // handle, never an error (concept Q4)
        let sys_unknown = patch_system_lines(0, 9, &[], &HashMap::new(), &first_seen);
        assert!(sys_unknown[0].1.text.contains("#9"), "{}", sys_unknown[0].1.text);
        assert_eq!(sys_unknown[0].0, 0, "never seen → sorts to the top");

        // merged by time into the chat lines; the chat order itself is
        // never disturbed and a tie puts the system line first
        let chat = vec![
            (100u64, line("me", "a")),
            (200, line("me", "b")),
            (300, line("me", "c")),
        ];
        let system = vec![
            (200u64, system_line_data("s2".into())),
            (150, system_line_data("s1".into())),
        ];
        let merged = merge_by_time(chat, system);
        assert_eq!(
            merged.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["a", "s1", "s2", "b", "c"]
        );
    }

    /// Review finding: the read contract's `pending` is Proposed-only, so
    /// the moment a proposal seals (or closes) it vanishes from every read
    /// and the patch channel degraded to "#id" with no state line. The
    /// UI-side cache must keep the title and resolve the fate from the
    /// applied log the UI already reads.
    #[test]
    fn patch_title_and_state_survive_the_proposal_leaving_pending() {
        let pv = ProposalView {
            id: ProposalId(4),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": "budget" }),
            approvals: 2,
            threshold: 3,
            state: ProposalState::Proposed,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: 0,
            declined_by: String::new(),
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        };
        let mut known = HashMap::new();
        // while pending: cached with title + progress
        update_known_proposals(&mut known, std::slice::from_ref(&pv), &[], &HashMap::new());
        assert_eq!(display_title(0, &known[&4].payload), "budget", "human title, no op-code prefix");
        assert_eq!(known[&4].fate, KnownFate::Pending);

        // the proposal leaves the Proposed-only window and its payload
        // shows up in the surface's applied log → Applied
        let applied = HashMap::from([(Surface::Memory, vec![pv.payload.clone()])]);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&4].fate, KnownFate::Applied);

        // the system line keeps the title and renders the sealed state
        let first_seen = HashMap::from([(4u64, 150u64)]);
        let sys = patch_system_lines(0, 4, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("budget") && text.contains('✓'), "{text}");
        assert!(text.contains("3/3"), "sealed at the threshold: {text}");

        // a sealed vote's discussion leaves the sidebar (discussions exist
        // to decide something — once decided there is nothing to vote on)
        let infos = vec![ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(4) },
            count: 1,
            last_ts: 10,
            state: None,
            unread: 0,
        }];
        let rows = derive_channels(0, &infos, &known, &HashMap::new());
        assert!(rows.is_empty(), "an Applied vote's discussion is hidden");

        // vanished WITHOUT an applied trace: the read contract cannot tell
        // Rejected from expired — neutral closed marker, title kept, no
        // fabricated verdict
        let pv9 = ProposalView {
            id: ProposalId(9),
            payload: serde_json::json!({ "title": "drop the fee" }),
            ..pv.clone()
        };
        update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &applied);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&9].fate, KnownFate::Closed);
        let sys = patch_system_lines(0, 9, &[], &known, &first_seen);
        let text = &sys[0].1.text;
        assert!(text.contains("drop the fee") && text.contains('⊘'), "{text}");
        assert!(!text.contains('✓') && !text.contains('✗'), "{text}");

        // an id never seen anywhere still tolerates (concept Q4)
        let sys = patch_system_lines(0, 77, &[], &known, &first_seen);
        assert_eq!(sys[0].1.text, "⚖ #77");

        // a Closed verdict corrects itself when the applied value shows up
        // in a later read (an out-of-order pass must not stick a wrong fate)
        let applied9 = HashMap::from([(
            Surface::Memory,
            vec![serde_json::json!({ "title": "drop the fee" })],
        )]);
        update_known_proposals(&mut known, &[], &[], &applied9);
        assert_eq!(known[&9].fate, KnownFate::Applied);
        // … while an already-Applied fate is sticky even if the surface
        // read is missing this pass
        update_known_proposals(&mut known, &[], &[], &HashMap::new());
        assert_eq!(known[&4].fate, KnownFate::Applied);
        assert_eq!(known[&9].fate, KnownFate::Applied);
    }

    /// One `ProposalView` for the cache tests, minimal noise.
    fn view_of(id: u64, title: &str, state: ProposalState) -> ProposalView {
        ProposalView {
            id: ProposalId(id),
            surface: Surface::Memory,
            payload: serde_json::json!({ "op": "add_note", "title": title }),
            approvals: 0,
            threshold: 3,
            state,
            approved_by_me: false,
            declined_by_me: false,
            current: String::new(),
            proposed: String::new(),
            votes: Vec::new(),
            declined_at: if state == ProposalState::Rejected { 100 } else { 0 },
            declined_by: if state == ProposalState::Rejected {
                "ashi".to_string()
            } else {
                String::new()
            },
            by: String::new(),
            mine: false,
            superseded: false,
            withdrawn: false,
        }
    }

    /// The snapshots' `declined` lists fold into the proposal cache: a veto
    /// this UI never saw pending (fresh open, another member's decline)
    /// still titles its discussion channel and flags it closed — and an
    /// Applied fate is never downgraded by the fold.
    #[test]
    fn declined_votes_fold_into_the_cache_as_closed() {
        let mut known = HashMap::new();
        // never seen pending: the decline inserts a Closed entry, titled
        let dv7 = view_of(7, "vetoed", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv7), &HashMap::new());
        assert_eq!(known[&7].fate, KnownFate::Closed);
        assert_eq!(display_title(0, &known[&7].payload), "vetoed", "human title from the summary");

        // a cached Pending refreshes to Closed when its decline shows up
        let pv8 = view_of(8, "late veto", ProposalState::Proposed);
        update_known_proposals(&mut known, std::slice::from_ref(&pv8), &[], &HashMap::new());
        assert_eq!(known[&8].fate, KnownFate::Pending);
        let dv8 = view_of(8, "late veto", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv8), &HashMap::new());
        assert_eq!(known[&8].fate, KnownFate::Closed);

        // an Applied fate is sticky against the fold (the applied-log probe
        // proved the seal; byte-identical-twin ambiguity must not un-seal)
        let pv9 = view_of(9, "sealed", ProposalState::Proposed);
        update_known_proposals(&mut known, std::slice::from_ref(&pv9), &[], &HashMap::new());
        let applied = HashMap::from([(Surface::Memory, vec![pv9.payload.clone()])]);
        update_known_proposals(&mut known, &[], &[], &applied);
        assert_eq!(known[&9].fate, KnownFate::Applied);
        let dv9 = view_of(9, "sealed", ProposalState::Rejected);
        update_known_proposals(&mut known, &[], std::slice::from_ref(&dv9), &applied);
        assert_eq!(known[&9].fate, KnownFate::Applied, "never downgraded");

        // …and the derive_channels contract holds over the folded cache:
        // the closed discussion stays OFF the sidebar
        let infos = vec![ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(7) },
            count: 2,
            last_ts: 20,
            state: Some(ProposalState::Rejected),
            unread: 0,
        }];
        assert!(
            derive_channels(0, &infos, &known, &HashMap::new()).is_empty(),
            "a declined vote's discussion is not a sidebar row"
        );
    }

    /// The decision-panel flag: only an ORGANIZATION decision's discussion.
    ///
    /// The ask is explicit that other surfaces' decisions are handled
    /// differently, so the panel must not appear for them. And it must not
    /// appear for the group chat or a free topic either — there is no
    /// decision to head those with.
    #[test]
    fn selected_channel_org_flags_only_organization_decisions() {
        let known_of = |surface: Surface| KnownProposal {
            payload: serde_json::json!({"op": "set_name", "value": "x"}),
            surface,
            approvals: 1,
            threshold: 2,
            fate: KnownFate::Pending,
        };
        let known = HashMap::from([
            (1u64, known_of(Surface::Organization)),
            (2u64, known_of(Surface::Memory)),
        ]);
        let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };

        assert!(selected_channel_org(&patch(1), &known), "an Organization decision");
        assert!(
            !selected_channel_org(&patch(2), &known),
            "another surface's decision is handled differently - no panel"
        );
        assert!(
            !selected_channel_org(&patch(9), &known),
            "an unknown referent heads nothing"
        );
        assert!(!selected_channel_org(&ChannelRef::Group, &known));
        assert!(!selected_channel_org(
            &ChannelRef::Topic { name: "budget".into() },
            &known
        ));
    }

    /// The compose-collapse flag: only a DECIDED vote's patch channel is
    /// read-only. The engine's enumeration annotation is authoritative when
    /// present; otherwise the proposal cache decides; group/topic, open
    /// votes and unknown referents (Q4) stay writable.
    #[test]
    fn selected_channel_closed_flags_only_decided_patch_votes() {
        let known_of = |fate: KnownFate| KnownProposal {
            payload: serde_json::json!({"op": "add_note", "title": "t"}),
            surface: Surface::Memory,
            approvals: 1,
            threshold: 2,
            fate,
        };
        let info = |id: u64, state: Option<ProposalState>| ChannelInfo {
            channel: ChannelRef::Patch { id: ProposalId(id) },
            count: 1,
            last_ts: 10,
            state,
            unread: 0,
        };
        let patch = |id: u64| ChannelRef::Patch { id: ProposalId(id) };
        let known = HashMap::from([
            (1u64, known_of(KnownFate::Pending)),
            (2u64, known_of(KnownFate::Closed)),
            (3u64, known_of(KnownFate::Applied)),
        ]);

        // group/topic are never closed
        assert!(!selected_channel_closed(&ChannelRef::Group, &[], &known));
        assert!(!selected_channel_closed(
            &ChannelRef::Topic { name: "x".into() },
            &[],
            &known
        ));

        // the engine annotation decides when present …
        let infos = vec![
            info(1, Some(ProposalState::Proposed)),
            info(2, Some(ProposalState::Rejected)),
            info(3, Some(ProposalState::Applied)),
        ];
        assert!(!selected_channel_closed(&patch(1), &infos, &HashMap::new()));
        assert!(selected_channel_closed(&patch(2), &infos, &HashMap::new()));
        assert!(selected_channel_closed(&patch(3), &infos, &HashMap::new()));
        // … and wins over a stale cache
        let stale = HashMap::from([(2u64, known_of(KnownFate::Pending))]);
        assert!(selected_channel_closed(&patch(2), &infos, &stale));

        // no (or unannotated) enumeration entry → the cache decides — the
        // instant-feedback path on selection passes no infos at all
        assert!(!selected_channel_closed(&patch(1), &[], &known));
        assert!(selected_channel_closed(&patch(2), &[], &known));
        assert!(selected_channel_closed(&patch(3), &[], &known));
        assert!(selected_channel_closed(&patch(2), &[info(2, None)], &known));

        // unknown everywhere stays writable (chat-bus Q4)
        assert!(!selected_channel_closed(&patch(99), &infos, &known));
    }

    /// The epoch invalidates a bundle read for a selection the user has
    /// LEFT — that is the whole job. It used to invalidate on every newer
    /// push start as well, which starved the pane (see
    /// `an_overlapping_push_does_not_starve_the_one_it_overlaps`): a stale
    /// bundle landing is a cosmetic revert one push later, an empty pane is
    /// the user losing their chat.
    #[test]
    fn push_generation_guard_invalidates_stale_pushes() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let g1 = st.begin_push("ws-1").expect("current");
        assert!(st.is_current(g1), "a push for the current selection lands");
        // a selection change invalidates every in-flight push …
        st.select(ChannelRef::Topic {
            name: "budget".into(),
        });
        assert!(!st.is_current(g1));
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".into()
            }
        );
        // … and the counter moves across the workspace-switch reset, so an
        // old push can never match a freshly reset state
        let g2 = st.begin_push("ws-1").expect("current");
        st.enter_workspace("ws-2");
        let g3 = st.begin_push("ws-2").expect("current");
        assert!(g3 > g2, "monotonic across enter_workspace resets");
        assert!(st.is_current(g3));
        assert!(!st.is_current(g2));
    }


    /// A workspace switch must not leak the previous workspace's channel
    /// state into the next one: a stale Patch/Topic selection would filter
    /// the new workspace's log until manually cleared, and the first-seen
    /// stamps would misplace system lines. Same workspace → everything is
    /// kept. (Unread counts live engine-side since B2 and reset with the
    /// workspace there.)
    #[test]
    fn chat_ui_state_resets_on_workspace_switch() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        st.selected = ChannelRef::Topic {
            name: "budget".to_string(),
        };
        st.first_seen.insert(4, 100);

        // the same workspace: selection and stamps survive
        st.enter_workspace("ws-1");
        assert_eq!(
            st.selected,
            ChannelRef::Topic {
                name: "budget".to_string()
            }
        );
        assert_eq!(st.first_seen.get(&4), Some(&100));

        // a switch: back to Group, stamps gone, and the new identity sticks
        st.enter_workspace("ws-2");
        assert_eq!(st.selected, ChannelRef::Group);
        assert!(st.first_seen.is_empty());
        st.selected = ChannelRef::Group;
        st.enter_workspace("ws-2");
        assert!(st.first_seen.is_empty(), "no reset without a switch");
    }

    #[test]
    fn channel_keys_round_trip() {
        for c in [
            ChannelRef::Group,
            ChannelRef::Patch { id: ProposalId(42) },
            ChannelRef::Topic { name: "Budget 2026".into() },
        ] {
            assert_eq!(parse_channel_key(&channel_key(&c)), Some(c));
        }
        assert_eq!(parse_channel_key("patch:xyz"), None, "junk never panics");
        assert_eq!(parse_channel_key(""), None);
    }

    #[test]
    fn charter_splits_into_balanced_columns_at_word_boundaries() {
        // a short charter stays single-column
        assert_eq!(
            charter_columns("kurz und knapp", 3),
            vec!["kurz und knapp".to_string()]
        );
        // empty → no columns (the UI shows its no-agenda line)
        assert!(charter_columns("   ", 3).is_empty());
        // ~450 chars → 2 columns; nothing lost, split at word boundaries
        let mid = "wort ".repeat(90);
        let cols = charter_columns(&mid, 3);
        assert_eq!(cols.len(), 2);
        assert!(
            cols.join(" ")
                .split_whitespace()
                .eq(mid.split_whitespace()),
            "columns are a display split - every word survives"
        );
        // a long charter caps at the column maximum
        let long = "wort ".repeat(300);
        assert_eq!(charter_columns(&long, 3).len(), 3);
        // umlauts near the cut never split a character
        let umlaut = "ä".repeat(400);
        let cols = charter_columns(&umlaut, 3);
        assert_eq!(cols.concat(), umlaut);
    }

    #[test]
    fn expires_labels_render_the_retention_deadline() {
        assert_eq!(expires_label(0, 100, 100 + 13 * 86_400, true), "in 13 days");
        assert_eq!(expires_label(0, 100, 100 + 86_400, true), "in 1 day");
        assert_eq!(expires_label(0, 100, 100 + 7_200, true), "in 2 h");
        assert_eq!(expires_label(0, 100, 100 + 120, true), "in 2 min");
        assert_eq!(expires_label(0, 500, 100, true), "expired");
        assert_eq!(
            expires_label(0, 100, 0, true),
            "-",
            "0 = unknown share age, no deadline (the engine keeps it forever)"
        );
        assert_eq!(
            expires_label(0, 100, 100 + 86_400, false),
            "-",
            "an unavailable share has nothing left to expire"
        );
        // the cell renders in the active language, like the tables around it
        assert_eq!(expires_label(1, 100, 100 + 13 * 86_400, true), "in 13 Tagen");
        assert_eq!(expires_label(1, 100, 100 + 86_400, true), "in 1 Tag");
        assert_eq!(expires_label(1, 500, 100, true), "abgelaufen");
    }

    #[test]
    fn quote_indent_groups_by_target_and_alternates_between_neighbors() {
        let mut log = vec![
            line("a", "question 1"),
            line("b", "reply 1"),
            line("c", "reply 2"),
            line("d", "reply to something else"),
            line("e", "plain"),
            line("f", "late reply"),
        ];
        log[1].quote_id = hex_id(1);
        log[2].quote_id = hex_id(1);
        log[3].quote_id = hex_id(2);
        log[5].quote_id = hex_id(3);
        let quotes = HashMap::from([
            (hex_id(1), qsrc("a", "question 1", false)),
            (hex_id(2), qsrc("x", "question 2", false)),
            (hex_id(3), qsrc("y", "question 3", false)),
        ]);
        annotate_chat_log(&mut log, &quotes);
        assert_eq!(log[0].quote_indent, 0, "no quote, no indent");
        assert_eq!(log[1].quote_indent, 1, "a fresh reply group starts at depth 1");
        assert_eq!(log[2].quote_indent, 1, "same target keeps the depth");
        assert_eq!(log[3].quote_indent, 2, "a neighboring different target alternates");
        assert_eq!(log[4].quote_indent, 0, "plain rows sit flush and end the run");
        assert_eq!(log[5].quote_indent, 1, "after a break the next group restarts at 1");
    }

    // ---- the chat pane's push epoch -----------------------------------

    /// **Two overlapping pushes must BOTH be able to land.**
    ///
    /// `push_surfaces` issues `MarkChannelRead` whenever the channel on
    /// screen has unread messages; the engine event that causes starts the
    /// next push while the current one is still reading. While `begin_push`
    /// bumped the epoch, that made the reading push stale and it threw its
    /// finished bundle away — so opening a chat with anything unread left
    /// the pane EMPTY until some later burst happened to leave one push
    /// unoverlapped. That is the bug this pins, and it is invisible to any
    /// test that pushes one at a time.
    #[test]
    fn an_overlapping_push_does_not_starve_the_one_it_overlaps() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let a = st.begin_push("ws-1").expect("the active workspace");
        let b = st.begin_push("ws-1").expect("the MarkChannelRead echo");
        assert!(st.is_current(b), "the newer push lands");
        assert!(
            st.is_current(a),
            "…and so does the one it overlapped: both read the same selection, \
             so dropping either renders nothing at all"
        );
    }

    /// **THE first-open bug, from the user's own log.**
    ///
    /// ```text
    /// ui: workspace switch from= to=752… gen=2
    /// ui: bundle gathered ws=752… gen=2 channel=group chat_rows=9
    /// ui: bundle DROPPED as stale gen=2
    /// ```
    ///
    /// The bundle was RIGHT — nine rows — and was thrown away 38 ms later
    /// because the epoch had moved. What moved it was the session mirror
    /// refreshing the CREATE WIZARD's relay picker: opening a workspace
    /// changes the dialable pool, `set_create_relays` bumped, and the
    /// surfaces bundle in flight died of it. Only on the first open,
    /// because the pool only changes once — which is exactly the reported
    /// symptom.
    ///
    /// The epoch is the SELECTION epoch. It exists so a bundle read for a
    /// channel or workspace the user has left cannot land. A relay picker
    /// the bundle does not even carry must not be able to invalidate it.
    #[test]
    fn unrelated_ui_state_cannot_stale_a_surfaces_bundle() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let in_flight = st.begin_push("ws-1").expect("current");

        // the session mirror refreshes the create wizard's relay picker —
        // which the surfaces bundle does not carry at all
        st.set_create_relays(vec!["wss://relay.example".to_string()]);
        assert!(
            st.is_current(in_flight),
            "the relay picker is not part of the bundle - it must not stale it"
        );

        // …and the things the bundle DOES carry still do
        let in_flight = st.begin_push("ws-1").expect("current");
        st.sort_members_by("name");
        assert!(
            !st.is_current(in_flight),
            "the members order IS in the bundle - a stale one would revert it"
        );
    }

    /// **A push reading for a workspace that is no longer open must not
    /// land, and must not drag the state back to it.**
    ///
    /// This is the empty chat on a first open. The workspace switch used to
    /// ride `begin_push`, keyed on whatever session copy that push had read
    /// — so a push that read the session BEFORE the open re-entered the
    /// state as "no workspace" AFTER it, bumped the epoch past the good
    /// push (whose bundle was then discarded) and landed its own empty one.
    /// Switching surfaces forced a fresh push, which is why it looked like
    /// the chat needed a nudge.
    #[test]
    fn a_push_that_read_the_session_before_an_open_cannot_land_after_it() {
        let mut st = ChatUiState::default();
        // …a push that read the session while nothing was open
        let stale = st.begin_push("").expect("nothing open is a state too");
        // …then the open lands, through the SESSION mirror
        st.enter_workspace("ws-1");
        let fresh = st.begin_push("ws-1").expect("the open workspace");

        assert!(st.is_current(fresh), "the push that read the open workspace lands");
        assert!(!st.is_current(stale), "…and the one from before it does not");
        // the decisive part: the stale push cannot re-enter the old state
        assert_eq!(
            st.begin_push(""),
            None,
            "a push for a workspace that is not open renders nothing at all"
        );
        assert_eq!(st.workspace, "ws-1", "…and it did not drag the state back");
    }

    /// The epoch exists for ONE thing: a bundle read for a selection the
    /// user has left must never land on the one they are looking at (it
    /// would also mark the wrong channel read).
    #[test]
    fn a_push_read_for_another_selection_never_lands() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-1");
        let in_flight = st.begin_push("ws-1").expect("current");
        st.select(ChannelRef::Topic { name: "budget".into() });
        assert!(
            !st.is_current(in_flight),
            "a bundle read for the previous channel must not land"
        );
        // …and a workspace switch is the same rule one level up
        let in_flight = st.begin_push("ws-1").expect("current");
        st.enter_workspace("ws-2");
        assert!(
            !st.is_current(in_flight),
            "a bundle read against another workspace's log must not land"
        );
    }

    // ---- the Restore wizard's one link field (welcome_rework.md) -------

    /// The two link shapes are rendered by the ENGINE's own `render()`,
    /// never hand-written here: a hand-built string pins the test's idea of
    /// the format, and the day the real one changes the test keeps passing
    /// while the panel stops recognizing anything.
    /// A real x-only anchor - the handover encoders validate the key, so a
    /// made-up hex string cannot stand in for one.
    fn anchor(seed: u8) -> String {
        molt_net::nostr_identity(&[seed; 32], "fixture").1
    }

    fn invite_link() -> String {
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
            republic: "Chess Club".to_string(),
            member: "petra".to_string(),
            ticket: "c".repeat(64),
            server: String::new(),
            queue_id: String::new(),
            wrap: String::new(),
            republic_id: "d".repeat(64),
            handover: Some(molt_net::invite::RecoveryHandoverV2 {
                identity_pk: String::new(),
                ticket: "c".repeat(64),
                npub: anchor(2),
                relays: vec!["ws://127.0.0.1:7777".to_string()],
                republic_id: "d".repeat(64),
            }),
        }
        .render()
    }

    /// One field, two flows: an invite link asks for a NAME and joins, a
    /// recovery link brings its own seat and needs the PHRASE. Getting this
    /// wrong sends someone through the founding ritual to recover a seat
    /// they already hold, so it is pinned rather than eyeballed.
    #[test]
    fn one_link_field_tells_a_join_from_a_recovery() {
        assert_eq!(
            link_kind(&invite_link()),
            LinkKind::Invite {
                republic: "Chess Club".to_string(),
                inviter: "walter".to_string(),
            },
            "a founding invite routes to the join"
        );
        assert_eq!(
            link_kind(&recovery_link()),
            LinkKind::Recovery {
                republic: "Chess Club".to_string(),
                member: "petra".to_string(),
            },
            "a recovery link routes to the ritual, and names its own seat"
        );
        // whitespace is what a paste actually carries
        assert_eq!(link_kind(&format!("  {}\n", invite_link())), link_kind(&invite_link()));
    }

    /// Everything else arms nothing. A PREVIEW-only invite link is the
    /// interesting case: it parses as a human-readable invite and carries no
    /// transport handover at all, so a panel that armed on "looks like an
    /// invite" would start a join that cannot reach anybody.
    #[test]
    fn a_link_that_cannot_act_arms_nothing() {
        let full = invite_link();
        let preview = full.rsplit_once('/').expect("the handover is the last segment").0;
        assert_eq!(
            link_kind(preview),
            LinkKind::Unrecognized,
            "a preview link has no transport handover - nothing can be done with it"
        );
        let damaged = format!("{}zz", recovery_link());
        assert_eq!(
            link_kind(&damaged),
            LinkKind::Unrecognized,
            "a damaged recovery handover is not an actionable link"
        );
        for junk in ["", "   ", "hello", "molt://", "molt://invite/", "https://example.com"] {
            assert_eq!(link_kind(junk), LinkKind::Unrecognized, "junk: {junk:?}");
        }
    }

    /// The chat offers exactly ONE view, and it is writable. The nav used
    /// to carry two more: an Archive (the older half of the retention
    /// window - an invisible cliff a conversation fell over at 3.5 days)
    /// and the agent-facing "unread" slice, which broke the pane outright:
    /// the GUI marks the on-screen channel read on every refresh, so it
    /// emptied itself on sight, and the compose row is gated on the general
    /// view, so there was nothing to write into either.
    #[test]
    fn the_chat_offers_one_writable_view() {
        assert_eq!(
            Surface::Chat.views().iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            ["today"],
            "a second chat view is a place a user can get stranded in"
        );
        assert_eq!(Surface::Chat.default_view(), "today");
        // …and the read slice stays available to an agent, off the nav
        assert!(molt_core::CHAT_READ_SLICES.contains(&"unread"));
    }

    #[test]
    fn when_label_relative_part() {
        let ts = 1_750_000_000_u64;
        let at = |offset: i64| when_label_at(0, ts, 1_750_000_000 + offset);
        assert!(at(5).ends_with("(just now)"), "{}", at(5));
        assert!(at(60).ends_with("(~1 minute ago)"), "{}", at(60));
        assert!(at(20 * 60).ends_with("(~20 minutes ago)"), "{}", at(1200));
        assert!(at(2 * 3600).ends_with("(~2 hours ago)"), "{}", at(7200));
        assert!(at(3 * 86_400).ends_with("(~3 days ago)"), "{}", at(259_200));
    }

    /// The presence cell reads a REAL stamp: fresh sightings stay relative,
    /// and past a week the DATE takes over - "34 d ago" is arithmetic the
    /// reader should not have to do. Only a seat this install has never had
    /// any evidence for says so.
    #[test]
    fn the_last_seen_cell_goes_from_relative_to_a_plain_date() {
        let now = 1_787_000_000_u64;
        assert_eq!(seen_label(0, now, molt_core::MemberInfo::NEVER, "never seen"), "never seen");
        assert_eq!(seen_label(0, now, now, ""), "just now");
        assert_eq!(seen_label(0, now, now - 3 * 3600, ""), "3 h ago");
        assert_eq!(seen_label(1, now, now - 2 * 86_400, ""), "vor 2 Tagen");
        // the week boundary: one side relative, the other the date itself
        assert_eq!(seen_label(0, now, now - 6 * 86_400, ""), "6 d ago");
        let old = now - 30 * 86_400;
        assert_eq!(seen_label(0, now, old, ""), date_label(0, old));
        assert_eq!(seen_label(1, now, old, ""), date_label(1, old));
        // the two spellings, pinned against the same instant
        let iso = date_label(0, old);
        let de = date_label(1, old);
        assert_eq!(iso.len(), 10, "ISO date: {iso}");
        assert_eq!(de.len(), 10, "German date: {de}");
        assert_eq!(
            de,
            format!("{}.{}.{}", &iso[8..10], &iso[5..7], &iso[0..4]),
            "the German date is the same day, written the German way"
        );
    }

    #[test]
    fn sync_status_label_matches_the_demo_prose() {
        assert_eq!(sync_status_label(0, 0, 0, 0), "Synced · just now");
        assert_eq!(sync_status_label(0, 0, 2, 0), "Synced · 2 min ago");
        assert_eq!(sync_status_label(0, 0, 60, 0), "Synced · 1 h ago");
        assert_eq!(sync_status_label(0, 1, 0, 80), "Syncing… 80 items left");
        assert_eq!(sync_status_label(0, 2, 4320, 0), "Offline · last sync 3 d ago");
    }

    #[test]
    fn nav_labels_speak_german() {
        assert_eq!(surface_name(1, Surface::Organization), "Organisation");
        assert_eq!(surface_name(0, Surface::Organization), "Organization");
        assert_eq!(view_label(1, "members", "Members"), "Mitglieder");
        assert_eq!(view_label(1, "archive", "Archive"), "Archiv");
        assert_eq!(view_label(1, "pending", "Pending"), "Ausstehend");
        // the Kanban views (kanban_workflows.md §6.0): "plan" is new,
        // "my-quests" keeps its wire key under the "Mine" label
        assert_eq!(view_label(1, "plan", "Planning"), "Planung");
        assert_eq!(view_label(0, "plan", "Planning"), "Planning");
        assert_eq!(view_label(1, "my-quests", "Mine"), "Meine");
        // unmapped keys fall back to the shared English vocabulary
        assert_eq!(view_label(1, "status", "Status"), "Status");
        assert_eq!(view_label(0, "members", "Members"), "Members");
    }

    #[test]
    fn sync_status_label_speaks_german() {
        assert_eq!(sync_status_label(1, 0, 0, 0), "Synchronisiert · gerade eben");
        assert_eq!(sync_status_label(1, 0, 2, 0), "Synchronisiert · vor 2 Min.");
        assert_eq!(sync_status_label(1, 0, 60, 0), "Synchronisiert · vor 1 Std.");
        assert_eq!(sync_status_label(1, 1, 0, 80), "Synchronisiere… 80 ausstehend");
        assert_eq!(
            sync_status_label(1, 2, 4320, 0),
            "Offline · letzter Sync vor 3 Tagen"
        );
        assert_eq!(sync_status_label(1, 0, 1440, 0), "Synchronisiert · vor 1 Tag");
    }

    fn ws(name: &str, minutes: i32) -> WorkspaceItem {
        WorkspaceItem {
            id: molt_core::demo_workspace_id(name).into(),
            name: name.into(),
            detail: "".into(),
            status: "".into(),
            synced: true,
            state: 0,
            last_sync_min: minutes,
            s3: false,
            backup: "".into(),
            encrypted: false,
            seed: "".into(),
            net: "".into(),
            members: ModelRc::new(VecModel::from(Vec::new())),
        }
    }

    #[test]
    fn size_and_backup_labels() {
        assert_eq!(size_label(920), "920 KiB");
        assert_eq!(size_label(1840), "1.8 MiB");
        assert_eq!(backup_when_label(0, molt_core::WorkspaceInfo::NEVER), "never");
        assert_eq!(backup_when_label(0, 0), "just now");
        assert_eq!(backup_when_label(0, 30), "30 min ago");
        assert_eq!(backup_when_label(0, 129_600), "90 d ago");
        assert_eq!(backup_when_label(1, molt_core::WorkspaceInfo::NEVER), "nie");
        assert_eq!(backup_when_label(1, 0), "gerade eben");
        assert_eq!(backup_when_label(1, 30), "vor 30 Min.");
        assert_eq!(backup_when_label(1, 129_600), "vor 90 Tagen");
    }

    /// A session with bucket-only entries, as a real listing would produce
    /// them: one true orphan (id only, no name) and one foreign key. The
    /// production DEFAULT has none — molt-core pins that.
    fn sv_with_orphans() -> SessionView {
        SessionView {
            backup_orphans: vec![
                molt_core::BackupOrphan {
                    id: "ab".repeat(32),
                    name: String::new(),
                    size_kib: 480,
                    last_backup_min: 129_600,
                },
                molt_core::BackupOrphan {
                    id: String::new(),
                    name: "molt/leftover.bin".to_string(),
                    size_kib: 75,
                    last_backup_min: 43_200,
                },
            ],
            // the demo republics are a fixture, not the default (review K6)
            workspaces: molt_core::WorkspaceInfo::demo_set(),
            ..SessionView::default()
        }
    }

    #[test]
    fn sort_bk_rows_by_size_and_names_with_empties_last() {
        let sv = sv_with_orphans();
        let mut rows = backup_rows(&sv);
        sort_bk_rows(&mut rows, "size", false);
        let sizes: Vec<i32> = rows.iter().map(|r| r.size_kib).collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]), "{sizes:?}");
        sort_bk_rows(&mut rows, "local", false);
        assert!(
            rows.last().expect("rows").local.is_empty(),
            "orphans sort last on the local column"
        );
        sort_bk_rows(&mut rows, "last", false);
        assert_eq!(
            rows.last().expect("rows").last.as_str(),
            "never",
            "never-backed-up rows sort last"
        );
    }

    #[test]
    fn backup_rows_map_locals_then_orphans() {
        let sv = sv_with_orphans();
        let rows = backup_rows(&sv);
        assert_eq!(rows.len(), sv.workspaces.len() + sv.backup_orphans.len());
        // locals first: name on the left, bucket side only when auto is on
        for (row, w) in rows.iter().zip(&sv.workspaces) {
            assert!(row.has_local);
            assert_eq!(row.local.as_str(), w.name);
            assert_eq!(row.auto, w.s3);
            // the bucket cell claims nothing the bucket didn't confirm: a
            // real backup error, else really listed copies, else empty —
            // never derived from the auto toggle alone (story 12 honesty)
            if w.backup_error.is_empty() && w.backup_copies == 0 {
                assert!(row.remote.is_empty());
            } else {
                assert!(!row.remote.is_empty());
            }
        }
        // orphans last: bucket side only, no toggle. A true orphan shows
        // its shortened workspace-id pseudonym (no name exists in the
        // bucket — never invent one); a foreign key shows its raw key.
        let orphans = &rows[sv.workspaces.len()..];
        for row in orphans {
            assert!(!row.has_local);
            assert_eq!(row.local.as_str(), "");
            assert!(!row.auto);
        }
        assert_eq!(orphans[0].remote.as_str(), "abababababab…");
        // the row keeps the FULL pseudonym (restore starts from it)
        assert_eq!(orphans[0].id.as_str(), "ab".repeat(32));
        assert_eq!(orphans[1].remote.as_str(), "molt/leftover.bin");
        assert_eq!(orphans[1].id.as_str(), "", "a foreign key has no workspace id");
    }

    /// The production default renders a table with ONLY the local rows —
    /// no invented bucket entries (story 8's regression fence, UI side).
    #[test]
    fn backup_rows_default_has_no_bucket_only_rows() {
        let sv = SessionView::default();
        let rows = backup_rows(&sv);
        assert_eq!(rows.len(), sv.workspaces.len());
        assert!(rows.iter().all(|r| r.has_local));
    }

    #[test]
    fn sort_ws_items_by_name_and_recency() {
        let mut items = vec![ws("beta", 60), ws("Alpha", 5), ws("gamma", 0)];
        sort_ws_items(&mut items, "name", false);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"], "case-insensitive");
        sort_ws_items(&mut items, "sync", false);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["gamma", "Alpha", "beta"], "most recent first");
        sort_ws_items(&mut items, "sync", true);
        let names: Vec<String> = items.iter().map(|w| w.name.to_string()).collect();
        assert_eq!(names, ["beta", "Alpha", "gamma"]);
    }

    /// Uploads-table row for the presentation tests. The DISPLAY strings
    /// are deliberately misleading (they would sort the other way round),
    /// pinning that date/size/expiry sort by the underlying numeric keys
    /// and never the rendered labels.
    fn upload(user: &str, name: &str, checksum: &str, ts: u64, bytes: u64) -> UploadRowData {
        UploadRowData {
            id: String::new(),
            user: user.to_string(),
            date: format!("{}", u64::MAX - ts),
            name: name.to_string(),
            kind: String::new(),
            size: format!("{} KiB", u64::MAX - bytes),
            available: true,
            online: true,
            // the cell shows a shortened prefix — the filter must still
            // match on the full value
            checksum: checksum.get(..4).unwrap_or(checksum).to_string(),
            expires: String::new(),
            status: String::new(),
            status_kind: 0,
            availability: String::new(),
            ts,
            bytes,
            expires_ts: ts,
            checksum_full: checksum.to_string(),
        }
    }

    #[test]
    fn sort_uploads_text_columns_case_insensitive() {
        let mut rows = vec![
            upload("bob", "zeta.pdf", "CC99", 1, 1),
            upload("Alice", "Alpha.PDF", "0b11", 2, 2),
            upload("carol", "beta.txt", "aa22", 3, 3),
        ];
        rows[0].kind = "PDF".to_string();
        rows[1].kind = "zip".to_string();
        rows[2].kind = "Txt".to_string();
        rows[0].status = "\u{2713}".to_string();
        rows[1].status = "42 %".to_string();
        let users = |rows: &[UploadRowData]| -> Vec<String> {
            rows.iter().map(|r| r.user.clone()).collect()
        };
        sort_uploads(&mut rows, "user", true);
        assert_eq!(users(&rows), ["Alice", "bob", "carol"], "case-insensitive");
        sort_uploads(&mut rows, "user", false);
        assert_eq!(users(&rows), ["carol", "bob", "Alice"], "descending flips");
        sort_uploads(&mut rows, "file", true);
        assert_eq!(users(&rows), ["Alice", "carol", "bob"], "Alpha < beta < zeta");
        sort_uploads(&mut rows, "type", true);
        assert_eq!(users(&rows), ["bob", "carol", "Alice"], "pdf < txt < zip");
        sort_uploads(&mut rows, "checksum", true);
        assert_eq!(users(&rows), ["Alice", "carol", "bob"], "0b < aa < cc");
        sort_uploads(&mut rows, "download", true);
        assert_eq!(users(&rows), ["carol", "Alice", "bob"], "idle < 42 % < ✓");
    }

    #[test]
    fn sort_uploads_numeric_columns_use_underlying_values() {
        // the rendered date/size labels would sort exactly the other way
        // round (see `upload`) — only the numeric keys give this order
        let mut rows = vec![
            upload("a", "x", "", 30, 10_240),
            upload("b", "y", "", 10, 2_048),
            upload("c", "z", "", 20, 900),
        ];
        let users = |rows: &[UploadRowData]| -> Vec<String> {
            rows.iter().map(|r| r.user.clone()).collect()
        };
        sort_uploads(&mut rows, "date", true);
        assert_eq!(users(&rows), ["b", "c", "a"], "oldest share first");
        sort_uploads(&mut rows, "date", false);
        assert_eq!(users(&rows), ["a", "c", "b"], "newest share first");
        sort_uploads(&mut rows, "size", true);
        assert_eq!(users(&rows), ["c", "b", "a"], "900 B < 2 KiB < 10 KiB");
        sort_uploads(&mut rows, "expires", true);
        assert_eq!(users(&rows), ["b", "c", "a"], "soonest expiry first");
        // an unknown/empty column keeps the current order
        sort_uploads(&mut rows, "", false);
        assert_eq!(users(&rows), ["b", "c", "a"]);
    }

    #[test]
    fn filter_uploads_matches_user_name_or_checksum_case_insensitively() {
        let all = || {
            vec![
                upload("Alice", "report.pdf", "aabb1122", 1, 1),
                upload("bob", "photo.png", "ccdd3344", 2, 2),
            ]
        };
        assert_eq!(filter_uploads(all(), "").len(), 2, "empty needle = all");
        let f = filter_uploads(all(), "LICE");
        assert_eq!(f.len(), 1, "user match, case-insensitive");
        assert_eq!(f[0].user, "Alice");
        let f = filter_uploads(all(), "PHOTO");
        assert_eq!(f.len(), 1, "filename match");
        assert_eq!(f[0].user, "bob");
        // beyond the 4-char display prefix — must match the FULL checksum
        let f = filter_uploads(all(), "DD33");
        assert_eq!(f.len(), 1, "full-checksum match");
        assert_eq!(f[0].user, "bob");
        assert!(filter_uploads(all(), "zzz").is_empty(), "no match = empty");
    }

    /// Members-table row for the sort tests.
    fn member(name: &str, id: &str, last_ts: u64, state: i32, uploads: i32) -> MemberRowData {
        MemberRowData {
            name: name.to_string(),
            id: id.to_string(),
            pk: id.to_string(),
            last: String::new(),
            last_ts,
            state,
            uploads,
            split: String::new(),
            image: String::new(),
            image_key: String::new(),
            desc: String::new(),
        }
    }

    #[test]
    fn sort_members_by_name_uploads_and_presence() {
        let mut rows = vec![
            member("bob", "0b", 10_000, 0, 3),
            member("Alice", "aa", 9_700, 1, 10),
            member("carol", "", 0, 2, 2),
        ];
        let names = |rows: &[MemberRowData]| -> Vec<String> {
            rows.iter().map(|r| r.name.clone()).collect()
        };
        sort_members(&mut rows, "name", true);
        assert_eq!(names(&rows), ["Alice", "bob", "carol"], "case-insensitive");
        sort_members(&mut rows, "uploads", true);
        assert_eq!(names(&rows), ["carol", "bob", "Alice"], "2 < 3 < 10 numeric");
        sort_members(&mut rows, "uploads", false);
        assert_eq!(names(&rows), ["Alice", "bob", "carol"]);
        // "last" is the REAL stamp: most recent first, never-seen (0) at
        // the end — regardless of pill state
        sort_members(&mut rows, "last", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"]);
        // unanchored (empty) identity cells sort last ascending
        sort_members(&mut rows, "id", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"], "0b < aa < empty");
        sort_members(&mut rows, "", true);
        assert_eq!(names(&rows), ["bob", "Alice", "carol"], "unknown = keep");
    }

    /// The Organization tables' view state: clicking the active column
    /// flips the direction, a new column starts ascending, and every
    /// change bumps the push generation (stales in-flight bundles).
    #[test]
    fn org_sort_state_toggles_and_bumps_generation() {
        let mut st = ChatUiState::default();
        let g = st.generation;
        st.sort_uploads_by("size");
        assert_eq!(st.uploads_sort, "size");
        assert!(st.uploads_asc, "a fresh column starts ascending");
        st.sort_uploads_by("size");
        assert!(!st.uploads_asc, "the same column flips the direction");
        st.sort_uploads_by("user");
        assert_eq!(st.uploads_sort, "user");
        assert!(st.uploads_asc, "switching columns resets to ascending");
        st.sort_members_by("uploads");
        assert_eq!(st.members_sort, "uploads");
        assert!(st.members_asc);
        st.set_uploads_filter("alice".to_string());
        assert_eq!(st.uploads_filter, "alice");
        assert_eq!(st.generation, g + 5, "every change stales in-flight pushes");
    }

    /// The pure paging window behind the proposal-outcome lists
    /// (Declined / the applied log): 20 rows per page, the page clamps
    /// into range (a shrunk list must never show an empty page), and a
    /// list of at most one page reports `page_count == 1` — the pager
    /// row hides on that.
    #[test]
    fn page_slice_windows_and_clamps() {
        // empty list: one (empty) page, never a panic range
        assert_eq!(page_slice(0, 0, 20), (0, 0, 0, 1));
        // exactly one page: untouched
        assert_eq!(page_slice(20, 0, 20), (0, 20, 0, 1));
        // one entry over: a second page holding the remainder
        assert_eq!(page_slice(21, 0, 20), (0, 20, 0, 2));
        assert_eq!(page_slice(21, 1, 20), (20, 21, 1, 2));
        // an out-of-range page clamps to the last one (the list shrank)
        assert_eq!(page_slice(21, 9, 20), (20, 21, 1, 2));
        // a full second page ends at the list end
        assert_eq!(page_slice(40, 1, 20), (20, 40, 1, 2));
        assert_eq!(page_slice(61, 3, 20), (60, 61, 3, 4));
    }

    /// The pager's UI-local state (ChatUiState, like the table sorts):
    /// prev/next step per (surface, list) independently, below-zero
    /// clamps at the first page, the push-time clamp re-bases a stored
    /// page against the list's current length (and writes it back, so
    /// the next step moves from the visible page), every step bumps the
    /// push generation, and a workspace switch resets everything.
    #[test]
    fn list_page_state_steps_clamps_and_resets() {
        let mut st = ChatUiState::default();
        st.enter_workspace("ws-a");
        let g = st.generation;
        st.page_list_by("organization", "declined", 1);
        st.page_list_by("organization", "declined", 1);
        assert_eq!(st.clamp_list_page("organization", "declined", 100), 2);
        assert_eq!(st.generation, g + 2, "every step stales in-flight pushes");
        // stepping below the first page clamps at zero
        st.page_list_by("organization", "declined", -9);
        assert_eq!(st.clamp_list_page("organization", "declined", 100), 0);
        // the clamp writes back: page 3 on a 2-page list re-bases to the
        // last page, and the next "prev" moves from THERE
        st.page_list_by("organization", "declined", 3);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 1);
        st.page_list_by("organization", "declined", -1);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
        // per-(surface, list) independence
        st.page_list_by("memory", "applied", 1);
        assert_eq!(st.clamp_list_page("memory", "applied", 100), 1);
        assert_eq!(st.clamp_list_page("organization", "declined", 30), 0);
        // a workspace switch resets the pages with the rest of the state
        st.enter_workspace("ws-b");
        assert_eq!(st.clamp_list_page("memory", "applied", 100), 0);
    }

    /// Guard: every nav sub-view of every surface has a real icon — the
    /// "▪️" fallback showing up in the sidebar means someone added a view
    /// without extending `view_icon`.
    #[test]
    fn every_view_has_an_icon() {
        for surface in Surface::ALL {
            for (key, _) in surface.views() {
                assert_ne!(view_icon(key), "▪️", "view `{key}` has no icon");
            }
        }
    }

    /// The tor-mode dropdown greys "embedded" unless the binary was built with
    /// the `embedded-tor` feature (P3). local + whonix are always selectable;
    /// only the middle (embedded) row tracks the compile-time truth passed
    /// through the app→ui seam.
    #[test]
    fn embedded_row_is_disabled_when_feature_off() {
        // model is ["local", "embedded", "whonix"]
        assert_eq!(tor_mode_enabled(false), [true, false, true]);
        assert_eq!(tor_mode_enabled(true), [true, true, true]);
    }

    /// The header "chat" pill mirrors transport health (P6): Ok → good/green
    /// with no tooltip; Degraded → warn/amber; Down → bad/red — the latter two
    /// carrying the engine's reason string as the hover tooltip.
    #[test]
    fn net_health_maps_to_pill_tone() {
        use molt_core::NetHealth;
        // tone index: 0 = good (green), 1 = warn (amber), 2 = bad (red)
        assert_eq!(net_health_pill(&NetHealth::Ok), (0, String::new()));
        assert_eq!(
            net_health_pill(&NetHealth::Degraded {
                reason: "Tor circuit timed out".to_string(),
            }),
            (1, "Tor circuit timed out".to_string()),
        );
        assert_eq!(
            net_health_pill(&NetHealth::Down {
                reason: "embedded Tor not built into this binary".to_string(),
            }),
            (2, "embedded Tor not built into this binary".to_string()),
        );
    }

    /// The workspace-folder browse dialog starts where the hand-editable
    /// draft points ONLY when that (after the engine's own `~` expansion —
    /// the config default is "~/…") is a real directory; anything else
    /// (empty draft, typo, a file) must yield no start dir so the dialog
    /// opens at its platform default instead of failing.
    #[test]
    fn ws_dir_browse_starts_at_the_draft_only_when_it_is_a_real_directory() {
        let dir = tempfile::tempdir().expect("create a temp directory");
        let dir_path = dir.path().display().to_string();
        assert_eq!(
            browse_start_dir(&dir_path),
            Some(dir.path().to_path_buf()),
            "an existing directory is a usable start dir"
        );
        // a "~" draft expands against $HOME exactly like the engine resolves
        // the setting — pinning the config default's "~/…" form to a REAL
        // start dir, not a literal "~" path that never exists
        let home = std::env::var_os("HOME").expect("HOME is set in the test env");
        assert_eq!(
            browse_start_dir("~"),
            Some(std::path::PathBuf::from(home)),
            "a tilde draft starts at the expanded home directory"
        );
        // a FILE is not a directory to start browsing in
        let file_path = dir.path().join("config.toml");
        std::fs::write(&file_path, b"x").expect("write a probe file");
        assert_eq!(browse_start_dir(&file_path.display().to_string()), None);
        assert_eq!(browse_start_dir(""), None, "empty draft → dialog default");
        assert_eq!(
            browse_start_dir(&format!("{dir_path}/definitely-missing")),
            None,
            "a stale/typoed draft → dialog default"
        );
    }
    /// The relay panel renders the ENGINE's verdict, never its own: every
    /// `blocked` reason becomes exactly one row state, and the position /
    /// end-of-list flags follow the pool order (which IS the priority).
    #[test]
    fn relay_rows_mirror_the_engine_verdict_and_the_priority_order() {
        let status = |url: &str, kind, confirmed, blocked| RelayStatus {
            url: url.to_string(),
            kind,
            confirmed,
            blocked,
        };
        let rows = relay_rows(&[
            // in use: a confirmed onion relay dials by itself
            status("wss://aaa.onion", RelayKind::Onion, true, None),
            // in the pool, but the user has not confirmed it
            status(
                "wss://relay.example.org",
                RelayKind::Clearnet,
                false,
                Some(RelayBlock::Unconfirmed),
            ),
            // confirmed local (LAN self-host), but this session has not
            // activated it — same gate as clearnet, own badge (kind 2)
            status(
                "ws://192.168.1.5:7777",
                RelayKind::Local,
                true,
                Some(RelayBlock::ClearnetSessionLocked),
            ),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pos, 1);
        assert_eq!(rows[0].kind, 0, "onion badge");
        assert!(rows[0].confirmed);
        assert_eq!(rows[0].blocked, 0, "no block = in use right now");
        assert!(rows[0].first, "position 0 cannot move up");
        assert!(!rows[0].last);
        assert_eq!(rows[1].pos, 2);
        assert_eq!(rows[1].kind, 1, "clearnet badge");
        assert_eq!(rows[1].blocked, 1, "unconfirmed");
        assert!(!rows[1].first && !rows[1].last, "the middle row moves both ways");
        assert_eq!(rows[2].pos, 3);
        assert_eq!(rows[2].kind, 2, "local badge - never presented as clearnet");
        assert_eq!(rows[2].blocked, 2, "outside Tor, not activated this session");
        assert!(rows[2].confirmed, "…yet confirmed: the two are independent");
        assert!(rows[2].last, "the bottom row cannot move down");
        // a single relay is BOTH ends — neither arrow may promise a move
        let one = relay_rows(&[status("wss://aaa.onion", RelayKind::Onion, false, Some(RelayBlock::Unconfirmed))]);
        assert!(one[0].first && one[0].last);
        assert!(relay_rows(&[]).is_empty(), "a fresh install shows no rows");
    }

    /// Every way the pool refuses a URL reaches the user as a readable line
    /// under the field — in their language, never as a silent no-op. The
    /// classification comes from molt-core's own parser, so the message and
    /// the engine's gate can never drift apart.
    #[test]
    fn a_refused_relay_url_gets_a_localized_message_under_the_field() {
        let pool = vec!["wss://relay.example.org".to_string()];
        for lang in [0, 1] {
            assert_eq!(
                relay_add_check(lang, "wss://fresh.example.org", &pool).as_deref(),
                Ok("wss://fresh.example.org")
            );
            assert!(
                relay_add_check(
                    lang,
                    "ws://abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion",
                    &pool
                )
                .is_ok(),
                "plaintext to an onion service is fine - Tor encrypts it"
            );
            // …and every refusal names its reason
            for bad in [
                "https://relay.example.org",
                "relay.example.org",
                "wss://",
                "ws://relay.example.org",
                "wss://relay example.org",
                // a .onion host that is not a real v3 address
                "wss://aaa.onion",
                // already in the pool (normalized: same relay, other spelling)
                "WSS://Relay.Example.ORG/",
            ] {
                let msg = relay_add_check(lang, bad, &pool)
                    .err()
                    .unwrap_or_else(|| panic!("{bad:?} must be refused with a message"));
                assert!(!msg.is_empty());
            }
        }
        // the five parser verdicts and the duplicate are DISTINCT messages,
        // so the user learns what to fix
        let msgs = [
            relay_add_check(0, "https://relay.example.org", &pool).err(),
            relay_add_check(0, "wss://", &pool).err(),
            relay_add_check(0, "ws://relay.example.org", &pool).err(),
            relay_add_check(0, "wss://relay example.org", &pool).err(),
            relay_add_check(0, "wss://aaa.onion", &pool).err(),
            relay_add_check(0, "wss://relay.example.org", &pool).err(),
        ];
        for (i, a) in msgs.iter().enumerate() {
            for b in msgs.iter().skip(i + 1) {
                assert_ne!(a, b, "each refusal reads differently");
            }
        }
        // German is a real translation, not the English string
        assert_ne!(
            relay_add_check(0, "wss://", &pool).err(),
            relay_add_check(1, "wss://", &pool).err(),
        );
    }

    /// The honesty invariant of the Tor probe, in colour: ONLY a proven
    /// circuit may read as "good". A SOCKS port that merely answers is amber
    /// (something is there, nothing is proven), and every rung that failed or
    /// refused is red or neutral — never green.
    #[test]
    fn only_a_proven_tor_circuit_is_toned_good() {
        use molt_core::TorTestState as S;
        assert_eq!(tor_test_tone(S::Circuit), TONE_GOOD);
        assert_eq!(tor_test_tone(S::ProxyOnly), TONE_WARN, "a listening port proves no circuit");
        for s in [S::Idle, S::Testing, S::Off] {
            assert_eq!(tor_test_tone(s), TONE_NEUTRAL, "{s:?} is not a verdict");
        }
        for s in [S::Misconfigured, S::NoProxy, S::NoTarget, S::CircuitFailed] {
            assert_eq!(tor_test_tone(s), TONE_BAD, "{s:?} is a failure");
        }
        for s in [
            S::Idle,
            S::Testing,
            S::Off,
            S::Misconfigured,
            S::NoProxy,
            S::ProxyOnly,
            S::NoTarget,
            S::CircuitFailed,
        ] {
            assert_ne!(tor_test_tone(s), TONE_GOOD, "{s:?} must never read as success");
        }
    }

    /// Every rung of the ladder reaches the user in their own language, and no
    /// two rungs share a sentence — the whole point is that the user learns
    /// WHICH rung was reached. The partial rung must say out loud that no
    /// circuit is proven.
    #[test]
    fn every_tor_rung_has_its_own_honest_copy_in_both_languages() {
        use molt_core::TorTestState as S;
        let all = [
            S::Idle,
            S::Testing,
            S::Off,
            S::Misconfigured,
            S::NoProxy,
            S::ProxyOnly,
            S::NoTarget,
            S::CircuitFailed,
            S::Circuit,
        ];
        for lang in [0, 1] {
            for (i, a) in all.iter().enumerate() {
                assert!(!tor_verdict_copy(lang, *a).is_empty(), "{a:?} needs copy");
                for b in all.iter().skip(i + 1) {
                    assert_ne!(
                        tor_verdict_copy(lang, *a),
                        tor_verdict_copy(lang, *b),
                        "{a:?} and {b:?} must not read the same"
                    );
                }
            }
            // German is a real translation, not the English string
            assert_ne!(tor_verdict_copy(0, *all.last().expect("non-empty")), tor_verdict_copy(1, *all.last().expect("non-empty")));
        }
        // the partial rung states the missing proof, in both languages
        assert!(
            tor_verdict_copy(0, S::ProxyOnly).contains("no circuit"),
            "EN must deny the circuit outright"
        );
        assert!(
            tor_verdict_copy(1, S::ProxyOnly).contains("Circuit"),
            "DE must deny the circuit outright"
        );
        // …and no rung short of Circuit may claim Tor works
        for s in all.iter().filter(|s| **s != S::Circuit) {
            let en = tor_verdict_copy(0, *s).to_lowercase();
            assert!(!en.contains("tor works"), "{s:?} must not claim Tor works");
        }
    }

    /// The technical second line never invents anything: it names only what
    /// the engine actually reported. A duration is shown for the rung it is
    /// meaningful on (the completed circuit) and nowhere else.
    #[test]
    fn the_tor_detail_line_states_only_what_was_probed() {
        use molt_core::{TorTest, TorTestState as S};
        assert_eq!(tor_test_detail(0, &TorTest::default()), "");
        let probed = TorTest {
            state: S::ProxyOnly,
            detail: "no confirmed relay to dial".into(),
            proxy: "127.0.0.1:9050".into(),
            target: String::new(),
            ms: 0,
        };
        let line = tor_test_detail(0, &probed);
        assert!(line.contains("127.0.0.1:9050"), "the probed SOCKS address is named");
        assert!(line.contains("no confirmed relay to dial"), "the engine's reason rides along");
        assert!(!line.contains("ms"), "no duration where none was measured");
        let circuit = TorTest {
            state: S::Circuit,
            detail: String::new(),
            proxy: "127.0.0.1:9050".into(),
            target: "wss://relay.onion".into(),
            ms: 812,
        };
        let line = tor_test_detail(0, &circuit);
        assert!(line.contains("wss://relay.onion"), "the relay that was reached is named");
        assert!(line.contains("812 ms"), "the circuit's dial time");
        // a duration measured on a rung that never completed a circuit is NOT
        // shown — it would read as a working connection
        let failed = TorTest { state: S::CircuitFailed, ms: 812, ..circuit.clone() };
        assert!(!tor_test_detail(0, &failed).contains("812 ms"));
    }

    /// The panel's button tests the DRAFT, not the saved settings: changing
    /// the anonymity network is restart-required, so the user will usually not
    /// have saved yet. The port is clamped into the wire type instead of
    /// wrapping — a garbage port must not silently become a valid one.
    #[test]
    fn the_tor_button_probes_the_draft_the_user_is_looking_at() {
        assert_eq!(tor_probe_args(0, 0, 9050), ("tor".to_string(), "local".to_string(), 9050));
        assert_eq!(
            tor_probe_args(0, 1, 9050),
            ("tor".to_string(), "embedded".to_string(), 9050)
        );
        assert_eq!(tor_probe_args(0, 2, 9050), ("tor".to_string(), "whonix".to_string(), 9050));
        // "none" is answered honestly by the engine (Off) — the GUI does not
        // silently rewrite it into a tor probe
        assert_eq!(tor_probe_args(1, 0, 9050), ("none".to_string(), "local".to_string(), 9050));
        // out-of-range drafts clamp to the "not given" marker, never wrap
        assert_eq!(tor_probe_args(0, 0, -1).2, 0);
        assert_eq!(tor_probe_args(0, 0, 70000).2, 0);
        assert_eq!(tor_probe_args(0, 0, 0).2, 0);
    }
}

#[cfg(test)]
mod s3_target_tests {
    //! The two S3 targets (`docs/storage/s3_buckets.md`): the byte quotas
    //! are edited in MiB but stored in bytes, and the two targets share no
    //! field on the way through the settings draft.

    use super::*;

    /// A quota the operator wrote by hand in bytes must survive a settings
    /// save that did not touch it - the MiB stepper is a VIEW of the value,
    /// not a re-quantization of it.
    #[test]
    fn an_untouched_byte_quota_is_not_rounded_onto_the_mib_grid() {
        // rounded UP, so the displayed limit is never smaller than the real one
        assert_eq!(mib_label(500_000_000), "477");
        assert_eq!(
            mib_text_to_bytes("477", 500_000_000),
            500_000_000,
            "the field still shows 477 - keep the exact stored bytes"
        );
        // …but a real edit converts
        assert_eq!(mib_text_to_bytes("1000", 500_000_000), 1000 * 1024 * 1024);
        // 0 is "no limit" on both sides, and clearing one really clears it
        assert_eq!(mib_label(0), "0");
        assert_eq!(mib_text_to_bytes("0", 0), 0);
        assert_eq!(mib_text_to_bytes("0", 500_000_000), 0);
        // an emptied field means no limit; garbage keeps the stored value
        // rather than inventing one
        assert_eq!(mib_text_to_bytes("  ", 500_000_000), 0);
        assert_eq!(mib_text_to_bytes("-5", 500_000_000), 500_000_000);
        assert_eq!(mib_text_to_bytes("abc", 500_000_000), 500_000_000);
        // an absurd number saturates instead of wrapping
        assert_eq!(mib_text_to_bytes(&u64::MAX.to_string(), 0), u64::MAX);
    }

    /// Push the account and both buckets into a real headless window and read
    /// the draft back: the two buckets stay distinct, and the quotas survive.
    #[test]
    fn both_buckets_round_trip_through_the_settings_draft() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let stored = SessionSettings {
            s3_endpoint: "https://backup.example.org".to_string(),
            s3_access_key: "BAK".to_string(),
            s3_secret_key: "bak-secret".to_string(),
            s3_bucket: "media-archive".to_string(),
            s3_max_bytes: 500_000_000,
            media_s3_bucket: "clips".to_string(),
            media_s3_max_bytes: 3 * 1024 * 1024 * 1024,
            ..SessionSettings::default()
        };
        apply_settings_fields(&ui, &stored);
        let draft = read_settings_draft(&ui, &stored);
        assert_eq!(draft.s3_endpoint, "https://backup.example.org");
        assert_eq!(draft.s3_bucket, "media-archive");
        assert_eq!(draft.media_s3_bucket, "clips");
        assert_eq!(
            draft.s3_access_key, "BAK",
            "one account: the credentials are not per bucket"
        );
        assert_eq!(
            draft.s3_max_bytes, 500_000_000,
            "the hand-written byte quota survives an untouched round trip"
        );
        assert_eq!(draft.media_s3_max_bytes, 3 * 1024 * 1024 * 1024);
        // and the form reports itself clean: an unedited draft must not make
        // the leave-guard fire
        assert!(
            !settings_draft_differs(&stored, &ui),
            "an untouched draft equals the stored settings"
        );
    }
}

#[cfg(test)]
mod gui_tests {
    //! **The GUI's own logic, run headless.**
    //!
    //! Everything here drives the REAL `AppWindow` against a REAL engine
    //! through the same live-mirror functions the running app uses — with
    //! `i-slint-backend-testing` there is no display and no window, so these
    //! belong in the ordinary suite.
    //!
    //! They exist because three chat bugs in a row were diagnosed by reading
    //! code instead of by evidence: the engine was provably right each time
    //! (checked against a live `moltd` over MCP), and the fault was in this
    //! layer, where nothing could observe it.

    use super::*;
    use molt_core::{ChannelRef, GroupConfig, Reply, SessionView, Surface};

    /// `gui_over_mcp.md` step 1's pin: the published snapshot claims what
    /// the WINDOW's models hold — screen, selection, the chat surface's
    /// row count and last bodies, the nav keys and the pending sum. The
    /// snapshot is the read half agents test the window through, so a
    /// drift here would make every such test lie.
    #[test]
    fn the_ui_snapshot_claims_what_the_window_holds() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_selected_view("today".into());
        ui.set_selected_channel("group".into());
        let log = ModelRc::new(VecModel::from(vec![
            LogLine { text: "erste".into(), ..LogLine::default() },
            LogLine { text: "zweite".into(), ..LogLine::default() },
            LogLine { text: "dritte".into(), ..LogLine::default() },
            LogLine { text: "vierte".into(), ..LogLine::default() },
        ]));
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![
            SurfaceTab {
                key: "chat".into(),
                log,
                pending_count: 0,
                ..SurfaceTab::default()
            },
            SurfaceTab { key: "organization".into(), pending_count: 2, ..SurfaceTab::default() },
        ])));
        let snap = build_ui_snapshot(&ui);
        assert_eq!(
            (snap.screen.as_str(), snap.surface.as_str(), snap.view.as_str(), snap.channel.as_str()),
            ("main", "chat", "today", "group")
        );
        assert_eq!(snap.chat_rows, 4, "the model's row count, not the engine's");
        assert_eq!(
            snap.chat_last,
            vec!["zweite".to_string(), "dritte".to_string(), "vierte".to_string()],
            "the last three rendered bodies"
        );
        assert_eq!(snap.nav, vec!["chat".to_string(), "organization".to_string()]);
        assert_eq!(snap.pending_count, 2);
        assert!(snap.compose_visible);
        let again = build_ui_snapshot(&ui);
        assert!(again.generation > snap.generation, "every publish bumps");
    }

    /// The wiki bridge drives the REAL generated `WikiState` face headless:
    /// open → edit → close → delete through the same callbacks the pane
    /// fires, asserting the models follow. This is the layer the unit tests
    /// in `wiki.rs` cannot see (types, models, borrow discipline).
    #[test]
    fn wiki_bridge_opens_edits_closes_and_deletes_headless() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let _wiki = wire_wiki(&ui);
        let g = ui.global::<WikiState>();
        // production starts EMPTY; the engine base arrives over the real
        // bridge (base-docs + base-arrived), exactly like the surfaces
        // mirror delivers it
        assert_eq!(g.get_tabs().row_count(), 0);
        assert!(!g.get_doc_open());
        g.set_base_docs(ModelRc::new(VecModel::from(vec![
            WikiBase {
                path: "charter.md".into(),
                content: "# Charter\n\nWhat we agreed to.".into(),
            },
            WikiBase {
                path: "glossary.md".into(),
                content: "# Glossary\n\nThe words we keep using.".into(),
            },
        ])));
        g.set_base_rev(1);
        g.invoke_base_arrived();
        assert_eq!(g.get_nav_rows().row_count(), 2, "the folded base lands");
        assert_eq!(g.get_cs_rows().row_count(), 0, "a clean tree has no panel");
        // open the charter via the open route so a tab exists
        let rows = g.get_nav_rows();
        let charter = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "charter.md")
            .expect("charter row");
        g.invoke_nav_open(charter.id);
        assert_eq!(g.get_tabs().row_count(), 1);
        assert!(g.get_doc_open());
        assert_eq!(g.get_doc_path().as_str(), "charter.md");
        // open glossary.md via the open route
        let rows = g.get_nav_rows();
        let glossary = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "glossary.md")
            .expect("glossary row");
        // a mark must PATCH the row model, never replace it: a swap
        // re-creates the row elements mid-double-click, which is exactly
        // how "double-click does not open" happened live
        g.invoke_nav_mark(glossary.id);
        let rows_after = g.get_nav_rows();
        assert!(
            std::ptr::eq(
                rows.as_any()
                    .downcast_ref::<VecModel<WikiNavRow>>()
                    .expect("nav rows are a VecModel") as *const _,
                rows_after
                    .as_any()
                    .downcast_ref::<VecModel<WikiNavRow>>()
                    .expect("still a VecModel") as *const _,
            ),
            "the nav model must survive a mark (rows patch in place)"
        );
        g.invoke_nav_open(glossary.id);
        assert_eq!(g.get_tabs().row_count(), 2);
        assert_eq!(g.get_doc_path().as_str(), "glossary.md");
        // a base refresh with the SAME content is a no-op for the models
        g.invoke_base_arrived();
        assert_eq!(g.get_tabs().row_count(), 2);
        assert_eq!(g.get_doc_path().as_str(), "glossary.md");
        // an edit turns up on the changeset stack, the tab status and the
        // preview diff
        g.invoke_edit_toggle();
        let edited = format!("{}\n\nA new closing thought.", g.get_raw());
        g.invoke_edited(edited.into());
        assert_eq!(g.get_cs_rows().row_count(), 1);
        let row = g.get_cs_rows().row_data(0).expect("stack row");
        assert_eq!(row.kind, 5, "an edit row");
        assert_eq!(row.label.as_str(), "glossary.md");
        assert!(g.get_cs_lines() > 0, "touched lines are counted");
        let tabs = g.get_tabs();
        let gtab = (0..tabs.row_count())
            .filter_map(|i| tabs.row_data(i))
            .find(|t| t.label.as_str() == "glossary.md")
            .expect("glossary tab");
        assert_eq!(gtab.status, 2, "the tab paints modified");
        g.invoke_edit_toggle();
        let blocks = g.get_blocks();
        assert!(
            (0..blocks.row_count())
                .filter_map(|i| blocks.row_data(i))
                .any(|b| b.status == 1 && b.text.as_str().contains("closing thought")),
            "the appended paragraph previews as Added"
        );
        // Ctrl+W closes glossary; focus falls back to the charter tab
        g.invoke_close_active();
        assert_eq!(g.get_tabs().row_count(), 1);
        assert_eq!(g.get_doc_path().as_str(), "charter.md");
        // Del on the marked (still glossary) row: a pending deletion — the
        // row stays, struck, and the chip carries both changes
        g.invoke_delete_marked();
        let rows = g.get_nav_rows();
        let struck = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|r| r.label.as_str() == "glossary.md")
            .expect("the deleted row stays listed");
        assert_eq!(struck.status, 3);
        // the stack narrates both actions, the NET counts only the delete
        assert_eq!(g.get_cs_rows().row_count(), 2);
        assert_eq!(g.get_cs_deleted(), 1);
        assert_eq!(g.get_cs_lines(), 0, "a deleted file's edits are not lines");
        // undo takes back the deletion (the edit stays pending) …
        g.invoke_cs_undo();
        assert_eq!(g.get_cs_rows().row_count(), 1);
        assert_eq!(g.get_cs_deleted(), 0);
        assert!(g.get_cs_lines() > 0);
        // … a per-file revert clears the file without touching others …
        g.invoke_nav_revert(struck.id);
        assert_eq!(g.get_cs_rows().row_count(), 0, "the panel is gone");
        // … and after fresh changes, revert-all clears everything at once
        g.invoke_new_file();
        g.invoke_new_folder();
        assert_eq!(g.get_cs_rows().row_count(), 2);
        assert_eq!(g.get_cs_added(), 1);
        g.invoke_cs_revert();
        assert_eq!(g.get_cs_rows().row_count(), 0);
        assert_eq!(g.get_cs_added(), 0);
    }

    /// **The 0px-collapse trap, pinned.** Seven of the nine poke sites wrap
    /// an existing Text in a `ContextMenuArea`, and three of those wrappers
    /// sit inside a LAYOUT — where an element contributes its children's
    /// size constraints or nothing at all. Nothing at all means an
    /// invisible, unclickable name. This measures the real geometry of the
    /// chat author's name after a live mirror pass.
    ///
    /// **Runs on the dev-ui chain only** — `ElementHandle` queries need the
    /// element names Slint keeps under `SLINT_EMIT_DEBUG_INFO`, which the
    /// interpreter path carries anyway while the code generator would put
    /// them into the ~400k-line module (a build that already peaks at ~9 GiB).
    /// The layout engine under test is the same in both paths. Run it with
    /// `CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 cargo test
    /// -p molt-ui --lib --features molt-ui/live-preview`.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_chat_author_name_keeps_its_width_inside_the_poke_menu_wrapper() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        ui.window()
            .set_size(slint::PhysicalSize::new(1200, 800));

        rt.block_on(async {
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();
            mirror(&w, &ui, &last, &chat_ui).await;
        });
        assert!(chat_rows(&ui) > 0, "no chat row, nothing to measure");
        // the repeaters only materialize on a shown window in the main screen
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.show().expect("show headless");

        let names: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-name")
                .collect();
        let menus: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "ChatRow::author-menu")
                .collect();
        assert!(!names.is_empty(), "the author header must render");
        assert_eq!(names.len(), menus.len(), "every name carries its menu area");
        for (n, m) in names.iter().zip(menus.iter()) {
            assert!(
                n.size().width > 1.0,
                "author name collapsed to {}px",
                n.size().width
            );
            // the CLICK area is what breaks silently: a wrapper that
            // contributes no size constraint is invisible to the pointer
            assert!(
                m.size().width >= n.size().width && m.size().height >= n.size().height,
                "menu area {}x{} does not cover the name {}x{}",
                m.size().width,
                m.size().height,
                n.size().width,
                n.size().height
            );
        }
    }
    /// **A right-click must actually OPEN the menu.** Every poke site is a
    /// `ContextMenuArea`; the operator reported that right-clicking does
    /// nothing anywhere, while the engine path is provably fine (a poke
    /// issued over MCP toasts on both nodes). This dispatches a REAL right
    /// press onto the chat author's menu area and looks for the menu item
    /// that must appear — checked to be ABSENT before the click, so a
    /// find that always matches cannot pass for a menu.
    ///
    /// **Runs on the dev-ui chain only** (element ids), like its geometry
    /// sibling above.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_right_click_on_a_poke_site_opens_the_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        rt.block_on(async {
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();
            mirror(&w, &ui, &last, &chat_ui).await;
        });
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        apply_strings(&ui, 0);
        ui.show().expect("show headless");
        // the author is the own seat in this fixture — make it a POKABLE
        // name so the area is enabled (the gate is what `Poke.can` decides)
        ui.global::<Poke>().set_me("petra".into());
        ui.global::<Poke>().set_on(true);

        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let menu = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "ChatRow::author-menu",
        )
        .next()
        .expect("the author menu area must render");
        right_click(&ui, &menu, 0.5);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click opened no menu carrying {label:?}"
        );
    }

    /// The SAME right-click, on the site the operator actually uses:
    /// Organization → Members. Its `ContextMenuArea` wraps the whole row,
    /// so the press must reach it wherever the row is not covered by a
    /// control of its own.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_right_click_on_a_member_row_opens_the_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(true);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        assert_eq!(rows.len(), 2, "one menu area per member row");
        // row 1 is petra — the pokable seat
        let area = &rows[1];
        assert!(
            area.size().width > 1.0 && area.size().height > 1.0,
            "the menu area collapsed to {:?}",
            area.size()
        );
        right_click(&ui, area, 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click on the member row opened no menu"
        );
    }

    /// **Poking off must not make the feature vanish.** An entry that is
    /// simply absent reads as a dead right-click (that is how the operator
    /// met it). With the switch off the menu still opens and names the
    /// action - greyed, so it says "this exists, it is off" instead of
    /// nothing at all.
    #[cfg(feature = "live-preview")]
    #[test]
    fn with_poking_off_the_member_row_still_offers_the_entry_greyed() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(false);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        right_click(&ui, &rows[1], 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "the entry must still be offered, greyed - `Poke.on` is what the \
             MenuItem binds its `enabled` to, and `can()` (pinned separately) \
             is what refuses the command"
        );
    }

    /// The own seat is never a poke target, switch or no switch: its row
    /// offers no menu at all.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_own_seats_row_offers_no_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = members_window(true);
        let label = ui.global::<Strings>().get_mem_poke().to_string();
        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(&ui, "AppWindow::om-row-menu")
                .collect();
        // row 0 is walter — this node's own seat
        right_click(&ui, &rows[0], 0.98);
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "the own seat must not offer the entry"
        );
    }

    /// **Organization → Accepted: the Value column must never overrun its
    /// cell.** A description change carries a whole sentence as its value;
    /// an unwrapped `Text` reports that whole line as its PREFERRED width,
    /// which pushes the row past the table instead of eliding inside it.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_long_accepted_value_elides_inside_its_cell() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("accepted".into());
        // a seat description is typed into a multi-line box, so its value
        // can carry NEWLINES — a table cell that renders them is three
        // lines tall inside a 40px row and paints over its neighbours
        let long = "Baut an der Autistenzentrale\nund schreibt die Protokolle,\n\
             erreichbar meistens nachts, Zeitzone egal, und noch ein Satz \
             damit die Zelle ganz sicher zu schmal wird";
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            applied_count: 1,
            accepted: ModelRc::new(VecModel::from(vec![ProposalRow {
                id: 1,
                text: "Member description".into(),
                proposed: long.into(),
                ..ProposalRow::default()
            }])),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        ui.show().expect("show headless");

        let table = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            &ui,
            "DecisionTable",
        )
        .next()
        .expect("the decided-votes table must render");
        let cell = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "DecisionTable::dt-value",
        )
        .next()
        .expect("the value cell must render");
        let right = cell.absolute_position().x + cell.size().width;
        let edge = table.absolute_position().x + table.size().width;
        eprintln!(
            "value cell {:?} w={} right={right} table right={edge}",
            cell.absolute_position(),
            cell.size().width
        );
        assert!(
            right <= edge,
            "the value ran {}px past the table",
            right - edge
        );
    }

    /// **The chat presence strip: the pill follows the name, and the
    /// last-seen label never leaves it.** A seat name is free text, and an
    /// unelided `Text` reports the WHOLE name as its preferred width - so a
    /// long name pushed itself and the last-seen label straight out of the
    /// fixed 150px pill (reported 2026-08-22). Two things are pinned: the
    /// column follows the longest name, and inside a pill the NAME is what
    /// gives way - the last-seen label stays visible.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_long_member_name_grows_its_pill_and_keeps_the_last_seen_label() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "chat".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);

        let seat = |name: &str, last: &str| MemberSync {
            name: name.into(),
            last: last.into(),
            state: 0,
        };
        let measure = |ui: &AppWindow| -> Vec<(f32, f32, f32)> {
            let pills: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                ui,
                "MemberPill",
            )
            .collect();
            let names: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-name")
                    .collect();
            let lasts: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "MemberPill::mp-last")
                    .collect();
            assert_eq!(pills.len(), names.len(), "every pill renders its name");
            assert_eq!(pills.len(), lasts.len(), "every pill renders its last-seen");
            pills
                .iter()
                .zip(names.iter())
                .zip(lasts.iter())
                .map(|((p, n), l)| {
                    let edge = p.absolute_position().x + p.size().width;
                    let name_right = n.absolute_position().x + n.size().width;
                    let last_right = l.absolute_position().x + l.size().width;
                    eprintln!(
                        "pill w={} right={edge} | name w={} right={name_right} | last w={} right={last_right}",
                        p.size().width,
                        n.size().width,
                        l.size().width
                    );
                    assert!(
                        name_right <= edge,
                        "the name ran {}px past its pill",
                        name_right - edge
                    );
                    assert!(
                        l.size().width > 1.0 && last_right <= edge,
                        "the last-seen label is {}px wide and ends {}px past the pill",
                        l.size().width,
                        last_right - edge
                    );
                    // and it is PARKED at the right edge, so the labels line
                    // up down the grid instead of drifting with the name
                    assert!(
                        last_right >= edge - 12.0,
                        "the last-seen label floats {}px short of the pill edge",
                        edge - last_right
                    );
                    (p.size().width, n.size().width, l.size().width)
                })
                .collect()
        };

        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat("ada", "2 min ago"),
            seat("bob", "just now"),
        ])));
        ui.show().expect("show headless");
        let short = measure(&ui);

        // a name of ordinary length still fits, but the pill has to GROW for
        // it instead of cutting it off at the 150px column
        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat("bartholomaeus-von-habsburg", "2 min ago"),
            seat("bob", "just now"),
        ])));
        let grown = measure(&ui);
        assert!(
            grown[0].0 > short[0].0 + 10.0,
            "the pill did not follow the name: {}px vs {}px",
            grown[0].0,
            short[0].0
        );

        // past every sane cap the NAME elides - the last-seen label stays
        // (measure() asserts it for every pill)
        ui.set_active_members(ModelRc::new(VecModel::from(vec![
            seat(&"x".repeat(300), "2 min ago"),
            seat("bob", "just now"),
        ])));
        let huge = measure(&ui);
        assert!(
            huge[0].1 < 2000.0,
            "the name was not elided: {}px",
            huge[0].1
        );

        // a cut name is not a lost name: hovering the pill spells it out in
        // the window-topmost hint overlay
        let pill = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
            &ui,
            "MemberPill",
        )
        .next()
        .expect("the strip renders its pills");
        let at = slint::LogicalPosition::new(
            pill.absolute_position().x + pill.size().width / 2.0,
            pill.absolute_position().y + pill.size().height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        assert_eq!(
            ui.global::<HintTip>().get_label().to_string(),
            "x".repeat(300),
            "the elided name must read in full on hover"
        );
    }

    /// **Every button must survive a bigger font.** The app font is a
    /// setting (9-28px); a button whose height or width is a hardcoded
    /// pixel count keeps the box of the 14px default while its label grows
    /// inside it - which is how the operator met a cut-off "Entschlüsseln"
    /// on the Open screen. Two invariants, measured on the real layout:
    /// a button's label stays INSIDE the button, and no two buttons
    /// overlap (a button taller than its fixed row lands on its neighbour).
    #[cfg(feature = "live-preview")]
    #[test]
    fn every_button_keeps_its_label_at_the_largest_font() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        apply_strings(&ui, 1); // German: the longest labels in the app
        // the biggest size the stepper offers
        ui.global::<Theme>().set_fs_app(28.0);
        ui.set_screen(AppScreen::Open);
        ui.set_ws_list(ModelRc::new(VecModel::from(vec![
            WorkspaceItem {
                id: "a".into(),
                name: "Erste Republik".into(),
                detail: "2-of-3".into(),
                status: "Synchronisiert".into(),
                synced: true,
                backup: "vor 30 Min.".into(),
                ..WorkspaceItem::default()
            },
            WorkspaceItem {
                id: "b".into(),
                name: "Zweite Republik".into(),
                detail: "3-of-5".into(),
                status: "Offline".into(),
                encrypted: true,
                backup: "nie".into(),
                ..WorkspaceItem::default()
            },
        ])));
        ui.show().expect("show headless");
        let mut checked = assert_buttons_scale(&ui, "open");
        assert!(checked > 3, "the Open screen must render its buttons");

        // the choice screen and the three wizards, step by step
        ui.set_screen(AppScreen::Choice);
        checked += assert_buttons_scale(&ui, "choice");
        for (screen, steps, set) in [
            (AppScreen::Create, 4, 0),
            (AppScreen::Join, 3, 1),
            (AppScreen::Restore, 4, 2),
        ] {
            ui.set_screen(screen);
            for step in 0..steps {
                match set {
                    0 => ui.set_cw_step(step),
                    1 => ui.set_jw_step(step),
                    _ => ui.set_rw_step(step),
                }
                checked += assert_buttons_scale(&ui, &format!("{screen:?} step {step}"));
            }
        }

        // the main screen, one pass per surface - WITH rows in them: the
        // buttons that sit inside chat rows, proposal cards and the members
        // table are exactly the ones a fixed row height would squash
        let log = ModelRc::new(VecModel::from(vec![
            LogLine {
                id: "aa".repeat(16).into(),
                lead: "bartholomaeus".into(),
                text: "Erste Nachricht in der Republik".into(),
                when: "2026-08-22 13:37 (gerade eben)".into(),
                first: true,
                quote: -1,
                patch_id: -1,
                ..LogLine::default()
            },
            LogLine {
                id: "bb".repeat(16).into(),
                lead: "petra".into(),
                text: "Zweite Nachricht".into(),
                when: "2026-08-22 13:38 (gerade eben)".into(),
                first: true,
                own: true,
                quote: -1,
                patch_id: -1,
                ..LogLine::default()
            },
        ]));
        let votes = ModelRc::new(VecModel::from(vec![
            ProposalRow {
                id: 1,
                text: "Mitgliedsbeschreibung ändern".into(),
                proposed: "ein neuer Satz".into(),
                ..ProposalRow::default()
            },
            ProposalRow {
                id: 2,
                text: "Relais aufnehmen".into(),
                proposed: "wss://relay.example".into(),
                ..ProposalRow::default()
            },
        ]));
        let surfaces: Vec<SurfaceTab> = ["chat", "organization", "memory", "vault", "kanban"]
            .iter()
            .map(|k| SurfaceTab {
                key: (*k).into(),
                log: log.clone(),
                pending: votes.clone(),
                accepted: votes.clone(),
                pending_count: 2,
                applied_count: 2,
                ..SurfaceTab::default()
            })
            .collect();
        ui.set_surfaces(ModelRc::new(VecModel::from(surfaces.clone())));
        ui.set_org_members(ModelRc::new(VecModel::from(vec![
            MemberRow {
                name: "bartholomaeus".into(),
                last: "vor 2 Min.".into(),
                ..MemberRow::default()
            },
            MemberRow {
                name: "petra".into(),
                last: "22.07.2026".into(),
                ..MemberRow::default()
            },
        ])));
        // the tables whose rows carry buttons - uploads, backups, the relay
        // pickers - are exactly the fixed-height rows a bigger font bursts
        ui.set_org_uploads(ModelRc::new(VecModel::from(vec![UploadRow {
            id: "cc".repeat(16).into(),
            name: "protokoll.pdf".into(),
            user: "bartholomaeus".into(),
            date: "2026-08-22".into(),
            kind: "PDF".into(),
            size: "1.2 MiB".into(),
            available: true,
            online: true,
            expires: "in 13 Tagen".into(),
            ..UploadRow::default()
        }])));
        ui.set_bk_rows(ModelRc::new(VecModel::from(vec![BackupRow {
            id: "a".into(),
            local: "Erste Republik".into(),
            remote: "erste.molt.enc".into(),
            has_local: true,
            size: "1.8 MiB".into(),
            ..BackupRow::default()
        }])));
        ui.set_cw_relay_picks(ModelRc::new(VecModel::from(vec![RelayPick {
            url: "wss://relay.example".into(),
            picked: true,
        }])));
        ui.set_screen(AppScreen::Main);
        for s in &surfaces {
            ui.set_selected_surface(s.key.clone());
            for view in ["", "members", "pending", "accepted", "today"] {
                ui.set_selected_view(view.into());
                checked += assert_buttons_scale(&ui, &format!("main/{}/{view}", s.key));
            }
        }

        // and the settings screen
        ui.set_screen(AppScreen::Settings);
        checked += assert_buttons_scale(&ui, "settings");
        // a sweep that silently stopped finding buttons proves nothing
        assert!(checked > 40, "only {checked} buttons were measured");
    }

    /// The measured invariants, so every screen can be checked the same
    /// way: a label inside its button, and no two buttons overlapping.
    #[cfg(feature = "live-preview")]
    fn assert_buttons_scale(ui: &AppWindow, screen: &str) -> usize {
        let buttons: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "AppButton")
                .filter(|b| b.size().width > 0.0 && b.size().height > 0.0)
                .collect();
        // the other controls sit in the SAME rows and scale by the same
        // token: a field that outgrows its row lands on the button next to
        // it, so they all go into the overlap check
        let controls: Vec<_> = ["AppField", "AppDropdown", "AppStepper", "AppCheck"]
            .iter()
            .flat_map(|t| {
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, t)
                    .filter(|c| c.size().width > 0.0 && c.size().height > 0.0)
                    .collect::<Vec<_>>()
            })
            .chain(buttons.iter().cloned())
            .collect();
        let labels: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_id(ui, "AppButton::abtn-label")
                .filter(|l| l.size().width > 0.0)
                .collect();
        let rect = |e: &i_slint_backend_testing::ElementHandle| {
            let p = e.absolute_position();
            let s = e.size();
            (p.x, p.y, p.x + s.width, p.y + s.height)
        };
        for l in &labels {
            let (lx0, ly0, lx1, ly1) = rect(l);
            // the button this label belongs to: the label sits on its
            // line, so take the button whose vertical span holds the
            // label's middle and whose left edge is the nearest one left
            // of the label (an overflowing label still STARTS inside)
            let mid = (ly0 + ly1) / 2.0;
            let owner = buttons
                .iter()
                .filter(|b| {
                    let (bx0, by0, _, by1) = rect(b);
                    bx0 <= lx0 + 0.5 && by0 <= mid && mid <= by1
                })
                .max_by(|a, b| {
                    rect(a)
                        .0
                        .partial_cmp(&rect(b).0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            let Some(owner) = owner else { continue };
            let (bx0, by0, bx1, by1) = rect(owner);
            assert!(
                lx1 <= bx1 + 0.5 && ly1 <= by1 + 0.5 && lx0 >= bx0 - 0.5 && ly0 >= by0 - 0.5,
                "{screen}: the label \"{}\" ({lx0},{ly0})-({lx1},{ly1}) breaks out of its \
                 button ({bx0},{by0})-({bx1},{by1})",
                l.accessible_label().unwrap_or_default()
            );
        }
        for (i, a) in controls.iter().enumerate() {
            for b in controls.iter().skip(i + 1) {
                let (ax0, ay0, ax1, ay1) = rect(a);
                let (bx0, by0, bx1, by1) = rect(b);
                let overlap = ax0 < bx1 - 0.5
                    && bx0 < ax1 - 0.5
                    && ay0 < by1 - 0.5
                    && by0 < ay1 - 0.5;
                assert!(
                    !overlap,
                    "{screen}: two controls overlap - {} ({ax0},{ay0})-({ax1},{ay1}) and \
                     {} ({bx0},{by0})-({bx1},{by1})",
                    a.type_name().unwrap_or_default(),
                    b.type_name().unwrap_or_default()
                );
            }
        }
        buttons.len()
    }

    /// **A hint must not outlive the pointer.** The nav rows write the
    /// window-topmost `HintTip` overlay on hover and clear it on leave -
    /// but the clear was guarded by comparing the tip's ANCHOR to the
    /// row's current position, so a row that moved while hovered (the nav
    /// expands its sub-views on a click, a list scrolls) could never
    /// recognize its own tip again and the bubble stayed on screen for
    /// good. Pinned here: leaving the row clears it, and so does leaving
    /// the window - even after the row moved underneath the pointer.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_nav_hint_disappears_when_the_pointer_leaves_the_row() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1000, 700));
        apply_strings(&ui, 1);
        // big font + long names: the nav label elides, which is what makes
        // the expanded row write a hint at all
        ui.global::<Theme>().set_fs_app(24.0);
        let views = ModelRc::new(VecModel::from(vec![
            ViewItem {
                key: "status".into(),
                name: "Status".into(),
                ..ViewItem::default()
            },
            ViewItem {
                key: "members".into(),
                name: "Mitglieder".into(),
                ..ViewItem::default()
            },
        ]));
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![
            SurfaceTab {
                key: "organization".into(),
                name: "Organisation der Republik".into(),
                views: views.clone(),
                ..SurfaceTab::default()
            },
            SurfaceTab {
                key: "chat".into(),
                name: "Unterhaltung und Beschlüsse".into(),
                views: views.clone(),
                ..SurfaceTab::default()
            },
        ])));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.show().expect("show headless");

        let rows: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
                .filter(|r| r.size().width > 0.0)
                .collect();
        assert!(rows.len() >= 2, "the nav must render its rows");
        // a hover change reaches the `changed` handlers on the next frame -
        // headless, that frame is `mock_elapsed_time` (it runs the change
        // trackers), which the real app gets for free from its render loop
        let frame = || {
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let hover = |ui: &AppWindow, e: &i_slint_backend_testing::ElementHandle| {
            let p = e.absolute_position();
            let s = e.size();
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(
                        p.x + s.width / 2.0,
                        p.y + s.height / 2.0,
                    ),
                });
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let leave = |ui: &AppWindow| {
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(900.0, 400.0),
                });
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
        };
        let tip = |ui: &AppWindow| ui.global::<HintTip>().get_label().to_string();

        // 1. plain enter/leave
        hover(&ui, &rows[1]);
        assert!(!tip(&ui).is_empty(), "hovering a cut nav label shows its hint");
        leave(&ui);
        assert_eq!(tip(&ui), "", "the hint must go when the pointer leaves");

        // 2. the row MOVES while hovered - a bigger font resizes every nav
        //    row, so the hovered one is somewhere else by the time the
        //    pointer leaves. This is the case the old anchor-guard could
        //    not clear (it compared the tip's anchor to the row's CURRENT
        //    position), and it is deliberately not one of the navigations
        //    that drop the hint outright.
        let before = rows[1].absolute_position().y;
        hover(&ui, &rows[1]);
        assert!(!tip(&ui).is_empty(), "hint is up again");
        ui.global::<Theme>().set_fs_app(20.0);
        frame();
        let moved: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SurfaceRow")
                .filter(|r| r.size().width > 0.0)
                .collect();
        assert_ne!(
            moved[1].absolute_position().y,
            before,
            "the fixture must actually move the row"
        );
        leave(&ui);
        assert_eq!(tip(&ui), "", "a hint whose row moved must still clear");

        // 3. the pointer leaves the WINDOW (the nav sits at the left edge,
        //    so this is the ordinary way out of it)
        hover(&ui, &moved[1]);
        assert!(!tip(&ui).is_empty(), "hint is up again");
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerExited);
        frame();
        assert_eq!(tip(&ui), "", "leaving the window must clear the hint");
    }

    /// **Organization -> Status: the gated-settings card.** Its rows are
    /// label + value + pencil inside a 300px card; an unelided label
    /// reports its whole line as the row's preferred width and shoves the
    /// pencil through the card's border (reported 2026-08-23). The label
    /// is what gives way, and the pencils line up on one right edge.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_org_settings_pencils_stay_inside_the_card_and_line_up() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        apply_strings(&ui, 1); // German: the long "Chat löschen nach" line
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("status".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            name: "Organisation".into(),
            ..SurfaceTab::default()
        }])));
        ui.set_org_chat_retention(30);
        ui.set_org_relays(ModelRc::new(VecModel::from(vec![
            slint::SharedString::from("wss://relay.example"),
        ])));
        ui.show().expect("show headless");

        for font in [14.0_f32, 24.0] {
            ui.global::<Theme>().set_fs_app(font);
            let card = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                &ui,
                "OrgSettingsCard",
            )
            .find(|c| c.size().width > 0.0)
            .expect("the gated-settings card must render");
            let edge = card.absolute_position().x + card.size().width;
            let pencils: Vec<_> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                    .filter(|b| {
                        let p = b.absolute_position();
                        b.size().width > 0.0
                            && p.x >= card.absolute_position().x - 1.0
                            && p.y >= card.absolute_position().y - 1.0
                            && p.y <= card.absolute_position().y + card.size().height + 1.0
                    })
                    .collect();
            assert_eq!(pencils.len(), 2, "font {font}: relays + retention pencil");
            let mut rights = Vec::new();
            for b in &pencils {
                let right = b.absolute_position().x + b.size().width;
                assert!(
                    right <= edge,
                    "font {font}: the pencil ran {}px through the card border",
                    right - edge
                );
                rights.push(right);
            }
            assert!(
                (rights[0] - rights[1]).abs() < 1.0,
                "font {font}: the pencils are not aligned: {rights:?}"
            );

            // and EVERY pencil in the pane's right-hand column shares that
            // edge - they read as one column, so a panel with its own
            // padding staggers visibly against its neighbours
            let column: Vec<f32> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "AppButton")
                    .filter(|b| {
                        let (w, h) = (b.size().width, b.size().height);
                        let right = b.absolute_position().x + w;
                        // square = one of the ✎ pencils, not a labelled button
                        w > 0.0 && (w - h).abs() < 1.0 && (edge - right).abs() < 60.0
                    })
                    .map(|b| b.absolute_position().x + b.size().width)
                    .collect();
            assert!(column.len() >= 4, "font {font}: found {} pencils", column.len());
            let (lo, hi) = column.iter().fold((f32::MAX, f32::MIN), |(lo, hi), r| {
                (lo.min(*r), hi.max(*r))
            });
            assert!(
                hi - lo < 1.0,
                "font {font}: the pencil column staggers by {}px: {column:?}",
                hi - lo
            );
        }
    }

    /// The pill CLIPS - a pane too narrow for even the elided name must not
    /// paint over its neighbour - and a clipped element must still hand its
    /// right-click to the poke menu, which renders in the window's popup
    /// layer rather than inside the pill.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_clipped_presence_pill_still_opens_its_poke_menu() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("chat".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "chat".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        let poke = ui.global::<Poke>();
        poke.set_on(true);
        poke.set_me("walter".into());
        ui.set_active_members(ModelRc::new(VecModel::from(vec![MemberSync {
            name: "ada".into(),
            last: "2 min ago".into(),
            state: 0,
        }])));
        ui.show().expect("show headless");

        let label = ui.global::<Strings>().get_mem_poke().to_string();
        assert!(!label.is_empty(), "the fixture must carry the menu title");
        assert!(
            poke_menu_open(&ui, &label).is_none(),
            "no menu may be findable before the click"
        );
        let pills: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "MemberPill")
                .collect();
        assert_eq!(pills.len(), 1, "the strip renders the seat");
        right_click(&ui, &pills[0], 0.5);
        assert!(
            poke_menu_open(&ui, &label).is_some(),
            "right-click on the presence pill opened no menu"
        );
    }

    /// The decided-votes table is an INDEX: one line per decision. A seat
    /// description is typed into a multi-line box (`Ich bin der Peter\n!`
    /// is what the operator's node actually holds), and rendering that
    /// newline made the 40px row two lines tall. The CARD keeps the real
    /// shape — sign-what-you-see reads the value as it will be applied.
    #[test]
    fn a_decided_rows_value_reads_as_one_line_while_the_card_keeps_the_shape() {
        let data = ProposalRowData {
            current: "Ich bin der Peter\n!".to_string(),
            proposed: "erste\n\nzweite   dritte".to_string(),
            ..ProposalRowData::default()
        };
        let row = to_decided_row(&data);
        assert_eq!(row.current.as_str(), "Ich bin der Peter !");
        assert_eq!(row.proposed.as_str(), "erste zweite dritte");
        let card = to_proposal_row(&data);
        assert!(
            card.proposed.as_str().contains('\n'),
            "the vote card must show the value as it will be applied"
        );
    }

    /// **The settings tab bar wraps, the titles do not.** A tab title must
    /// never break inside its own tab; when the row cannot hold them all,
    /// the BAR takes a second row instead. Measured on the real geometry at
    /// two window widths.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_settings_tabs_stay_one_line_and_the_bar_wraps_when_it_must() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Settings);
        ui.set_active_workspace("w".into());
        apply_strings(&ui, 1); // German — the widest titles
        ui.window().set_size(slint::PhysicalSize::new(1600, 900));
        ui.show().expect("show headless");

        let rows_at = |ui: &AppWindow| -> Vec<f32> {
            let mut ys: Vec<f32> =
                i_slint_backend_testing::ElementHandle::find_by_element_type_name(ui, "SettingsTab")
                    .map(|t| t.absolute_position().y)
                    .collect();
            ys.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
            ys.dedup_by(|a, b| (*a - *b).abs() < 1.0);
            ys
        };
        let tabs: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
                .collect();
        assert_eq!(tabs.len(), 9, "nine tabs with a workspace open");
        for t in &tabs {
            assert!(
                t.size().height <= 30.0,
                "a tab grew to {}px - its title wrapped inside the tab",
                t.size().height
            );
        }
        assert_eq!(rows_at(&ui).len(), 1, "1600px holds every tab in one row");

        // …and narrow enough, the BAR breaks instead of the titles
        ui.window().set_size(slint::PhysicalSize::new(700, 900));
        let narrow: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SettingsTab")
                .collect();
        assert_eq!(narrow.len(), 9, "no tab may disappear when the bar wraps");
        for t in &narrow {
            assert!(
                t.size().height <= 30.0,
                "a tab grew to {}px in the narrow bar",
                t.size().height
            );
        }
        assert_eq!(rows_at(&ui).len(), 2, "700px needs a second row");
    }

    /// **Settings: the S3 credentials have their own tab.** "Backup" used
    /// to carry two errands at once - WHEN/WHICH workspace is backed up,
    /// and WHERE the bucket is. The endpoint, keys and bucket moved to
    /// "S3 config"; the schedule stayed. Driven by real clicks on the real
    /// bar (the tabs are found by type and ordered left to right - the
    /// bar's transparent measuring texts carry the same titles, so looking
    /// tabs up by their label would hit those instead).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_s3_endpoint_moved_out_of_the_backup_tab_onto_its_own() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.set_screen(AppScreen::Settings);
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1600, 900));
        ui.show().expect("show headless");

        let shown = |ui: &AppWindow, label: &str| {
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label)
                .next()
                .is_some()
        };
        let click_tab = |ui: &AppWindow, index: usize| {
            let mut tabs: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_type_name(
                ui,
                "SettingsTab",
            )
            .collect();
            tabs.sort_by(|a, b| {
                a.absolute_position()
                    .x
                    .partial_cmp(&b.absolute_position().x)
                    .expect("no NaN")
            });
            let tab = tabs.get(index).expect("the tab must render");
            let pos = tab.absolute_position();
            let size = tab.size();
            let at = slint::LogicalPosition::new(
                pos.x + size.width / 2.0,
                pos.y + size.height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
        };

        click_tab(&ui, 2); // Backup
        assert!(shown(&ui, "Automatic S3 backup"), "the schedule stays on Backup");
        assert!(!shown(&ui, "S3 endpoint"), "the endpoint left the Backup tab");
        assert!(!shown(&ui, "Access key"), "so did the keys");

        click_tab(&ui, 3); // S3 config
        assert!(shown(&ui, "S3 endpoint"), "the endpoint lives on the S3 tab");
        assert!(shown(&ui, "Access key"), "and so do the keys");
        assert!(shown(&ui, "Bucket"), "and the bucket");
        assert!(!shown(&ui, "Automatic S3 backup"), "the schedule did not follow");
    }

    /// The Vault mock's secrets list, headless: the deposits render, and a
    /// click on "Seal a secret" opens the dialog it was given (the button
    /// used to be dead, with a "not yet" tooltip).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_vault_seal_button_opens_its_dialog() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1400, 900));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("vault".into());
        ui.set_selected_view("secrets".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "vault".into(),
            ..SurfaceTab::default()
        }])));
        apply_strings(&ui, 0);
        ui.show().expect("show headless");

        let cards: Vec<_> =
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "SecretCard")
                .collect();
        eprintln!("secret cards: {}", cards.len());
        assert!(cards.len() >= 6, "the sample deposits must render");
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
                .next()
                .is_none(),
            "no dialog before the click"
        );

        let button = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "VaultPane::vt-seal-btn",
        )
        .next()
        .expect("the seal button must render");
        let pos = button.absolute_position();
        let size = button.size();
        let at = slint::LogicalPosition::new(
            pos.x + size.width / 2.0,
            pos.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
            position: at,
            button: slint::platform::PointerEventButton::Left,
        });
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_element_type_name(&ui, "ConfirmModal")
                .next()
                .is_some(),
            "the seal dialog must open"
        );
    }

    /// Organization → Members with two seats, rendered headless. `on` is
    /// the applied poke switch the menus gate on.
    #[cfg(feature = "live-preview")]
    fn members_window(on: bool) -> AppWindow {
        let ui = AppWindow::new().expect("headless window");
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        ui.set_screen(AppScreen::Main);
        ui.set_selected_surface("organization".into());
        ui.set_selected_view("members".into());
        ui.set_surfaces(ModelRc::new(VecModel::from(vec![SurfaceTab {
            key: "organization".into(),
            ..SurfaceTab::default()
        }])));
        ui.set_node_member("walter".into());
        ui.set_org_members(ModelRc::new(VecModel::from(vec![
            MemberRow { name: "walter".into(), ..MemberRow::default() },
            MemberRow { name: "petra".into(), ..MemberRow::default() },
        ])));
        ui.global::<Poke>().set_me("walter".into());
        ui.global::<Poke>().set_on(on);
        apply_strings(&ui, 0);
        ui.show().expect("show headless");
        ui
    }

    /// One real right press inside `area`, `fx` across its width (1.0 = the
    /// right edge) and vertically centred.
    #[cfg(feature = "live-preview")]
    fn right_click(ui: &AppWindow, area: &i_slint_backend_testing::ElementHandle, fx: f32) {
        let pos = area.absolute_position();
        let size = area.size();
        let at = slint::LogicalPosition::new(
            pos.x + (size.width * fx).min(size.width - 2.0),
            pos.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
        ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
            position: at,
            button: slint::platform::PointerEventButton::Right,
        });
    }

    /// The open poke menu, found by the title its single item carries.
    #[cfg(feature = "live-preview")]
    fn poke_menu_open(
        ui: &AppWindow,
        label: &str,
    ) -> Option<i_slint_backend_testing::ElementHandle> {
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, label).next()
    }

    /// The poke gate lives in ONE place (`Poke.can`, theme.slint) because
    /// nine sites render a member name and each offers the menu. This pins
    /// what every one of them inherits: off means no menu anywhere, the own
    /// seat is never a target, and an empty name (system lines, tombstone-
    /// free rows) never is either.
    #[test]
    fn the_poke_gate_refuses_the_own_seat_the_empty_name_and_the_off_switch() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let poke = ui.global::<Poke>();
        poke.set_me("walter".into());

        poke.set_on(false);
        assert!(!poke.invoke_can("petra".into()), "off: no menu anywhere");

        poke.set_on(true);
        assert!(poke.invoke_can("petra".into()), "on: another seat pokable");
        assert!(!poke.invoke_can("walter".into()), "never the own seat");
        assert!(!poke.invoke_can("".into()), "no name, no target");
    }

    /// The menus gate on the APPLIED switch, never the settings draft: a
    /// ticked-but-unsaved checkbox would offer a menu the engine refuses.
    #[test]
    fn the_poke_gate_follows_the_applied_setting_not_the_draft() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            settings: molt_core::SessionSettings {
                poke_enabled: true,
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert!(ui.global::<Poke>().get_on(), "applied switch reaches the menus");

        // the draft alone must not move it
        ui.set_cfg_poke_enabled(false);
        assert!(ui.global::<Poke>().get_on(), "the draft does not gate the menu");
    }

    /// The rejoiner's checklist (recovery_auto_approval.md §5): the session's
    /// `RecoverState` becomes per-seat rows plus the have/need counters, and
    /// an empty state clears the rows again.
    #[test]
    fn the_recover_checklist_maps_seats_and_counts() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            recover: molt_core::RecoverState {
                member: "petra".to_string(),
                need: 3,
                seats: vec![
                    molt_core::RecoverSeat { member: "walter".to_string(), approved: true },
                    molt_core::RecoverSeat { member: "petra".to_string(), approved: true },
                    molt_core::RecoverSeat { member: "vera".to_string(), approved: false },
                ],
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (2, 3));
        let rows = ui.get_rv_seats();
        let got: Vec<(String, bool)> = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .map(|r| (r.member.to_string(), r.approved))
            .collect();
        assert_eq!(
            got,
            vec![
                ("walter".to_string(), true),
                ("petra".to_string(), true),
                ("vera".to_string(), false)
            ],
            "roster order, per-seat approval"
        );

        // a fresh recovery clears the list (RecoverStart resets the state)
        apply_session(&ui, &SessionView::default(), true, &chat_ui);
        assert_eq!(ui.get_rv_seats().row_count(), 0);
        assert_eq!((ui.get_rv_have(), ui.get_rv_need()), (0, 0));
    }

    /// Restore-from-backup (recovery_auto_approval.md §7): the Settings ›
    /// Backup modal's state machine — a confirm without a phrase starts
    /// nothing; with one it hands ("s3", the orphan's id, the phrase) to the
    /// real restore pipeline, leads to the Restore screen, and drops the
    /// phrase. Runs on the dev-ui chain (`ElementHandle` needs the
    /// interpreter's debug info).
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_backup_restore_modal_drives_the_s3_pipeline() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let calls: Rc<RefCell<Vec<(String, String, String)>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let c = calls.clone();
            ui.on_restore_start(move |way, target, secret| {
                c.borrow_mut().push((way.to_string(), target.to_string(), secret.to_string()));
            });
        }
        let navs: Rc<RefCell<Vec<AppScreen>>> = Rc::new(RefCell::new(Vec::new()));
        {
            let n = navs.clone();
            ui.on_navigate(move |s| n.borrow_mut().push(s));
        }
        let label = ui.global::<Strings>().get_bk_restore().to_string();
        assert!(!label.is_empty(), "the label must be applied before searching");
        ui.show().expect("show headless");
        // control BEFORE the modal: nothing wears the label on this screen
        assert!(
            i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, label.as_str())
                .next()
                .is_none(),
            "no restore affordance before the modal opens"
        );
        // an orphan row opened the modal
        ui.set_bk_restore_id("cafe01".into());
        ui.set_bk_restore_open(true);
        let click = |ui: &AppWindow| {
            let h = i_slint_backend_testing::ElementHandle::find_by_accessible_label(
                ui,
                label.as_str(),
            )
            .next()
            .expect("the modal's confirm button renders");
            let at = slint::LogicalPosition::new(
                h.absolute_position().x + h.size().width / 2.0,
                h.absolute_position().y + h.size().height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
            ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                position: at,
                button: slint::platform::PointerEventButton::Left,
            });
        };
        // no phrase yet: the confirm is disarmed
        click(&ui);
        assert!(calls.borrow().is_empty(), "no phrase, no pipeline");
        assert!(ui.get_bk_restore_open(), "the modal stays up");
        // with the phrase: the REAL pipeline is asked, the modal closes,
        // the phrase is dropped, and the run view is next
        ui.set_bk_restore_seed("brave mole over the hills".into());
        click(&ui);
        assert_eq!(
            calls.borrow().as_slice(),
            &[(
                "s3".to_string(),
                "cafe01".to_string(),
                "brave mole over the hills".to_string()
            )],
            "confirm hands way/target/phrase to restore-start"
        );
        assert!(!ui.get_bk_restore_open(), "confirm closes the modal");
        assert_eq!(ui.get_bk_restore_seed().as_str(), "", "every way out drops the phrase");
        assert_eq!(navs.borrow().last(), Some(&AppScreen::Restore), "the run view is next");
    }

    /// One orphan-row session for the backup-table tests (field bug
    /// 2026-08-24): a bucket-only workspace plus one foreign key.
    fn sv_backup_orphan() -> (SessionView, String) {
        let id = "ab".repeat(32);
        let sv = SessionView {
            backup_orphans: vec![
                molt_core::BackupOrphan {
                    id: id.clone(),
                    name: String::new(),
                    size_kib: 480,
                    last_backup_min: 60,
                },
                molt_core::BackupOrphan {
                    id: String::new(),
                    name: "molt/leftover.bin".to_string(),
                    size_kib: 75,
                    last_backup_min: 43_200,
                },
            ],
            // no demo locals: exactly the two bucket rows render
            workspaces: Vec::new(),
            ..SessionView::default()
        };
        (sv, id)
    }

    /// The orphan row's restore affordance lives IN the local column (user
    /// decision 2026-08-24) and must FIT the row — the old trailing button
    /// sat beyond the table's column budget and was clipped invisible on
    /// every build (the "kein Knopf zu sehen" field report), worse under
    /// ui-scale. Measured at a scaled app font, dev-ui chain.
    #[cfg(feature = "live-preview")]
    #[test]
    fn the_orphan_restore_button_sits_in_the_local_column_and_fits() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.global::<Theme>().set_fs_app(20.0); // ui-scale ≈ 1.43, the field setup
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let (sv, _id) = sv_backup_orphan();
        apply_session(&ui, &sv, true, &chat_ui);
        ui.set_screen(AppScreen::Settings);
        ui.set_set_tab(2);
        ui.show().expect("show headless");
        let btn = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bkr-btn",
        )
        .next()
        .expect("the orphan row renders its restore button");
        let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bk-row",
        )
        .collect();
        assert_eq!(rows.len(), 2, "orphan + foreign row render");
        let row = &rows[0];
        let btn_right = btn.absolute_position().x + btn.size().width;
        let row_right = row.absolute_position().x + row.size().width;
        assert!(
            btn.size().width > 0.0 && btn_right <= row_right + 0.5,
            "the restore button must fit inside its row: button right {btn_right} vs row right {row_right}"
        );
        // …and the last COLUMN stays inside too (the pre-fix budget ignored
        // the ui-scale of the fixed columns, clipping the row's tail)
        for r in &rows {
            assert!(
                r.absolute_position().x + r.size().width <= 1200.0,
                "a row never overflows the window"
            );
        }
    }

    /// A double-click anywhere on an orphan row arms the same restore modal
    /// as the button (user decision 2026-08-24); a foreign-key row (no
    /// workspace id) stays inert.
    #[cfg(feature = "live-preview")]
    #[test]
    fn a_double_click_on_an_orphan_row_arms_the_restore_modal() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        apply_strings(&ui, 0);
        ui.window().set_size(slint::PhysicalSize::new(1200, 800));
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let (sv, id) = sv_backup_orphan();
        apply_session(&ui, &sv, true, &chat_ui);
        ui.set_screen(AppScreen::Settings);
        ui.set_set_tab(2);
        ui.show().expect("show headless");
        let rows: Vec<_> = i_slint_backend_testing::ElementHandle::find_by_element_id(
            &ui,
            "AppWindow::bk-row",
        )
        .collect();
        let dclick = |row: &i_slint_backend_testing::ElementHandle| {
            let at = slint::LogicalPosition::new(
                // between the columns, not on the button
                row.absolute_position().x + row.size().width * 0.6,
                row.absolute_position().y + row.size().height / 2.0,
            );
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::PointerMoved { position: at });
            for _ in 0..2 {
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerPressed {
                    position: at,
                    button: slint::platform::PointerEventButton::Left,
                });
                ui.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: at,
                    button: slint::platform::PointerEventButton::Left,
                });
            }
        };
        // the foreign-key row (sorted last) stays inert
        dclick(&rows[1]);
        assert!(!ui.get_bk_restore_open(), "a foreign key has nothing to restore");
        // the orphan row arms the modal with ITS id
        dclick(&rows[0]);
        assert!(ui.get_bk_restore_open(), "double-click arms the restore modal");
        assert_eq!(ui.get_bk_restore_id().as_str(), id, "the row's own id");
    }

    /// A node with storage, a founded workspace, and chat in it — the state
    /// a user's second launch starts from.
    fn node_with_chat(root: &std::path::Path) -> (WalletHandle, String) {
        // exactly the session `moltd` hands the engine at startup: the
        // workspaces are what is ON DISK. `SessionView::default()` carries
        // the demo fixtures, which would list six republics that do not
        // exist and hide the one that does.
        let session = SessionView {
            workspaces: molt_storage::scan_workspaces(root)
                .iter()
                .map(molt_storage::ScanEntry::info)
                .collect(),
            settings: molt_core::SessionSettings {
                workspace_dir: root.display().to_string(),
                ..molt_core::SessionSettings::default()
            },
            ..SessionView::default()
        };
        let w = molt_engine::spawn_with_storage(GroupConfig::demo(), session);
        (w, String::new())
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// How many chat rows the window is showing right now.
    fn chat_rows(ui: &AppWindow) -> usize {
        ui.get_surfaces()
            .iter()
            .find(|s| s.key == "chat")
            .map_or(0, |s| s.log.row_count())
    }

    /// A sealed workspace ON DISK, demo-grade (empty identities and
    /// attestations), plus the unix `now` its appended events should stamp
    /// — NOW, not a fixed stamp: chat older than the retention window is
    /// correctly invisible, and a fixture from last year would "reproduce"
    /// a bug that is the product working as specified.
    fn workspace_on_disk(
        root: &std::path::Path,
        rule_m: u8,
        roster: &[&str],
        agenda: &str,
    ) -> (molt_storage::OpenedWorkspace, u64) {
        let phrase = molt_storage::generate_seed_phrase().expect("phrase");
        let seed = molt_storage::seed_entropy(&phrase).expect("entropy");
        let sealed = molt_core::SealedRoster {
            name: "DevTest".to_string(),
            republic_id: "d0".repeat(32),
            rule_m,
            rule_n: u8::try_from(roster.len()).expect("roster fits u8"),
            roster: roster.iter().map(|s| (*s).to_string()).collect(),
            identities: Vec::new(),
            attestations: Vec::new(),
            relays: Vec::new(),
            agenda: agenda.to_string(),
            features: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let genesis = sealed.into_genesis(roster[0], now);
        let ws = molt_storage::create_workspace(root, &seed, &genesis).expect("create");
        (ws, now)
    }

    /// The live-mirror's own two steps (session push, then surfaces
    /// gather + apply), in its own order. The apply runs DIRECTLY rather
    /// than through `invoke_from_event_loop`: the headless backend never
    /// drains that queue, and the hop onto the UI thread is Slint's
    /// business, not this layer's.
    async fn mirror(
        w: &WalletHandle,
        ui: &AppWindow,
        last: &Arc<Mutex<Option<SessionSettings>>>,
        chat_ui: &Arc<Mutex<ChatUiState>>,
    ) {
        let weak = ui.as_weak();
        push_session(w, &weak, last, SessionScope::Full, chat_ui).await;
        if let Some((_, b)) = gather_surfaces(w, chat_ui).await {
            apply_surfaces(ui, &b);
        }
    }

    /// **THE reported sequence: a cold start, then OPEN a workspace that is
    /// already on disk.**
    ///
    /// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
    /// ich muss auf organization klicken und wieder zurück" — the switch is
    /// what the second push stands for, and the assertion is BEFORE it.
    #[test]
    fn a_cold_open_of_a_stored_workspace_fills_the_chat_pane() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        // --- a workspace ON DISK, the way a previous run left one behind
        let (mut ws, now) = workspace_on_disk(&root, 1, &["walter"], "test the chat");
        // …with a message in it
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 1,
            seq: 2,
            ts: now,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([7u8; 16]),
                "walter",
                "hello group",
                now,
            )),
        })
        .expect("append");
        ws.sync().expect("sync");
        drop(ws);

        // --- second run: a COLD app, the way the user starts it
        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            // the app comes up on the Choice screen and mirrors once
            mirror(&w, &ui, &last, &chat_ui).await;
            assert_eq!(
                chat_rows(&ui),
                0,
                "nothing is open yet - if this is not empty the test proves nothing"
            );

            // …and then the user opens the workspace
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            let open_id = stored;
            w.execute(Command::OpenWorkspace { id: open_id })
                .await
                .expect("the stored workspace opens");
            // the engine's own answer first: if IT is empty, the fault is
            // not in this layer and the assertion below would blame the
            // wrong one
            let engine_rows = match w
                .execute(Command::ReadState {
                    surface: Surface::Chat,
                    channel: Some(molt_core::ChannelRef::Group),
                    view: None,
                })
                .await
            {
                Ok(Reply::State(snap)) => snap.applied.len(),
                _ => 0,
            };
            assert_eq!(engine_rows, 1, "the engine holds the stored message");

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            chat_rows(&ui) > 0,
            "opening a stored workspace must fill the chat pane - having to \
             visit another surface and come back IS the bug"
        );
    }

    /// **The reported bug: opening a workspace must fill the chat pane.**
    ///
    /// "beim ersten öffnen eines workspaces wird ein leerer chat angezeigt,
    /// ich muss auf organization klicken und wieder zurück" — so the test
    /// asserts the pane after the OPEN, before any surface switch.
    #[test]
    fn opening_a_workspace_fills_the_chat_pane() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter(); // the engine spawns tasks at construction
        let (w, _) = node_with_chat(tmp.path());
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));

        rt.block_on(async {
            // found a session-only workspace and say something in it
            w.execute(Command::CreateStart {
                name: "DevTest".to_string(),
                member: "walter".to_string(),
                threshold: 1,
                members: 1,
                relays: Vec::new(),
            })
            .await
            .ok();
            w.execute(Command::Chat {
                body: "hello group".to_string(),
                quote: None,
                channel: ChannelRef::Group,
            })
            .await
            .ok();

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            ui.get_surfaces().row_count() > 0,
            "the bundle must have landed at all (else this test proves nothing)"
        );
        assert!(
            chat_rows(&ui) > 0,
            "the chat pane must hold the message the engine has - it took a \
             surface switch to appear, which is the reported bug"
        );
    }

    /// **The reported bug: a member wrote into a fresh topic and the two
    /// RECEIVING clients stopped reacting — "klick auf linke navbar Chat
    /// zeigt keine Funktion".**
    ///
    /// Receiver perspective, headless: the workspace holds a group message
    /// and a FOREIGN member's message in a topic channel (arrived over the
    /// wire, so it is unread here). The mirror must survive that state, the
    /// nav must list the topic row, and the Chat click must keep working.
    #[test]
    fn a_foreign_topic_message_keeps_the_chat_usable() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        let (mut ws, now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the chat");
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 1,
            seq: 2,
            ts: now,
            by: "walter".to_string(),
            body: molt_core::WorkspaceEvent::Chat(molt_core::ChatMessage::text(
                molt_core::MessageId([7u8; 16]),
                "walter",
                "hello group",
                now,
            )),
        })
        .expect("append group message");
        // the foreign topic message, the way the wire landed it
        ws.append(&molt_core::EventEnvelope {
            prev_seq: 2,
            seq: 3,
            ts: now,
            by: "ingrid".to_string(),
            body: molt_core::WorkspaceEvent::Chat(
                molt_core::ChatMessage::text(
                    molt_core::MessageId([9u8; 16]),
                    "ingrid",
                    "topic talk",
                    now,
                )
                .with_channel(ChannelRef::Topic {
                    name: "asdf".to_string(),
                }),
            ),
        })
        .expect("append topic message");
        ws.sync().expect("sync");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            mirror(&w, &ui, &last, &chat_ui).await;

            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            // the mirror push that follows the delivery — the receivers
            // froze HERE if this layer chokes on the topic state
            mirror(&w, &ui, &last, &chat_ui).await;
            assert!(
                chat_rows(&ui) > 0,
                "the group log must still show after a topic message arrived"
            );
            assert!(
                ui.get_chat_channels().iter().any(|c| c.key == "topic:asdf"),
                "the nav must list the foreign topic's row"
            );

            // …and the user's Chat click still navigates
            w.execute(Command::SelectSurface {
                surface: Surface::Chat,
            })
            .await
            .expect("the chat click reaches the engine");
            mirror(&w, &ui, &last, &chat_ui).await;
        });

        assert!(
            chat_rows(&ui) > 0,
            "after clicking Chat the pane must keep its rows - a dead pane \
             IS the reported bug"
        );
    }

    /// **The set_relays vote card shows the CHANGES** (relay story,
    /// 2026-08-09): a pending pool edit reaches the window as a relay-op
    /// card carrying the diff rows — kept, added, removed — instead of the
    /// generic Ist/Soll text pair.
    #[test]
    fn a_pool_edit_proposal_carries_the_diff_rows() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();

        // one seat cannot drive an edit to applied at m=2 (the proposer
        // already counts as approver), so the effective pool stays empty
        // here and every proposed relay renders as ADDED — the
        // kept/removed semantics are pinned by `relay_pool_diff`'s unit
        // test against a non-empty Ist-Stand
        let (ws, _now) = workspace_on_disk(&root, 2, &["walter", "ingrid"], "test the pool");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            // the pool edit stays pending at m=2 — the vote card under test
            w.execute(Command::Propose {
                surface: Surface::Organization,
                payload: serde_json::json!({
                    "op": "set_relays",
                    "value": "wss://kept.example wss://new.example",
                }),
            })
            .await
            .expect("the pool edit proposes");

            mirror(&w, &ui, &last, &chat_ui).await;
        });

        let org = ui
            .get_surfaces()
            .iter()
            .find(|s| s.key == "organization")
            .expect("org surface present");
        assert_eq!(org.pending.row_count(), 1, "the pool edit is pending");
        let card = org.pending.row_data(0).expect("card row");
        assert!(card.relay_op, "the card knows it is a pool edit");
        let rows: Vec<(i32, String)> = card
            .relay_changes
            .iter()
            .map(|c| (c.sign, c.url.to_string()))
            .collect();
        assert_eq!(
            rows,
            vec![
                (RELAY_ROW_ADDED, "wss://kept.example".to_string()),
                (RELAY_ROW_ADDED, "wss://new.example".to_string()),
            ],
            "the card carries the pool diff (empty Ist-Stand: all added)"
        );
    }

    /// **The reported bug (2026-08-09): after an approval elsewhere applied
    /// the vote, clicking Chat showed "ein kaputtes Panel mit leerem
    /// 'Proposal:', das die Hälfte der Seite einnimmt".**
    ///
    /// A decided vote's discussion stays a selectable read-only view, but
    /// the decision header's lookup chained only pending + declined — an
    /// APPLIED proposal is in neither list, so the card above the chat
    /// rendered from `ProposalRow::default()`: the empty wreck. The header
    /// must carry the decided card.
    #[test]
    fn a_decided_votes_discussion_keeps_its_decision_card() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let rt = rt();
        let _guard = rt.enter();
        let (ws, _now) = workspace_on_disk(&root, 1, &["walter"], "test the header");
        drop(ws);

        let (w, _) = node_with_chat(&root);
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let last: Arc<Mutex<Option<SessionSettings>>> = Arc::new(Mutex::new(None));
        rt.block_on(async {
            let stored = molt_storage::scan_workspaces(&root)
                .first()
                .map(|e| e.info().id)
                .expect("the workspace is on disk");
            w.execute(Command::OpenWorkspace { id: stored })
                .await
                .expect("the stored workspace opens");
            mirror(&w, &ui, &last, &chat_ui).await;
            // the vote APPLIES instantly at m=1 — the state right after
            // the approval sound on the reporting client
            w.execute(Command::Propose {
                surface: Surface::Organization,
                payload: serde_json::json!({ "op": "set_name", "value": "NewName" }),
            })
            .await
            .expect("the vote proposes and applies");
            // …and the user opens the decision's discussion
            chat_ui
                .lock()
                .expect("ui state")
                .select(ChannelRef::Patch {
                    id: molt_core::ProposalId(1),
                });
            mirror(&w, &ui, &last, &chat_ui).await;
        });

        let card = ui.get_selected_decision();
        assert!(
            card.id == 1 && !card.text.is_empty(),
            "a decided vote's discussion must head with ITS card, never an \
             empty one (id={}, text={:?})",
            card.id,
            card.text
        );
    }

    // ---- wiki export (docs_archive/memory/wiki_export_plan.md, keystone 5) -------

    /// The 💾 button writes the APPROVED tree, so the gate is the folded
    /// base - never the local stack, which the export deliberately leaves
    /// behind. One place decides it (`WikiState.has-base`), because the
    /// toolbar button and the dialog must never disagree.
    #[test]
    fn the_wiki_export_button_follows_the_approved_base_tree() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let g = ui.global::<WikiState>();

        g.set_base_docs(ModelRc::new(VecModel::from(Vec::<WikiBase>::new())));
        assert!(!g.invoke_has_base(), "an empty base has nothing to export");

        // a local draft alone must NOT arm the button: drafts stay local
        g.set_cs_rows(ModelRc::new(VecModel::from(vec![WikiChangeRow {
            kind: 0,
            label: "notes.md".into(),
        }])));
        assert!(!g.invoke_has_base(), "a local draft is not an approved tree");

        g.set_base_docs(ModelRc::new(VecModel::from(vec![WikiBase {
            path: "charter.md".into(),
            content: "hello".into(),
        }])));
        assert!(g.invoke_has_base(), "one approved doc arms the export");
    }

    /// The dialog's drafts line appears only when there IS a local stack -
    /// telling a user with nothing pending that nothing pending stays local
    /// is noise.
    #[test]
    fn the_export_dialog_counts_only_a_non_empty_local_stack() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let g = ui.global::<WikiState>();

        g.set_cs_rows(ModelRc::new(VecModel::from(Vec::<WikiChangeRow>::new())));
        assert_eq!(g.invoke_draft_count(), 0, "no stack, no line");

        g.set_cs_rows(ModelRc::new(VecModel::from(vec![
            WikiChangeRow {
                kind: 0,
                label: "a.md".into(),
            },
            WikiChangeRow {
                kind: 5,
                label: "b.md".into(),
            },
        ])));
        assert_eq!(g.invoke_draft_count(), 2, "the line names the real count");
    }

    /// The outcome toast is built from the engine's own export state, in
    /// both languages, and stays silent while the export is idle or still
    /// running (a toast per session push would repeat forever).
    #[test]
    fn the_wiki_export_toast_carries_the_real_outcome() {
        let idle = molt_core::ExportState::default();
        assert!(
            super::wiki_export_toast(0, &idle).is_none(),
            "nothing happened yet"
        );

        let running = molt_core::ExportState {
            running: true,
            dest: "/tmp/x".to_string(),
            ..molt_core::ExportState::default()
        };
        assert!(
            super::wiki_export_toast(0, &running).is_none(),
            "no verdict while it runs"
        );

        let ok = molt_core::ExportState {
            result: "ok".to_string(),
            files: 12,
            ..molt_core::ExportState::default()
        };
        let (msg, failed) = super::wiki_export_toast(0, &ok).expect("a verdict");
        assert!(!failed);
        assert_eq!(msg, "wiki exported: 12 files");
        let (de, _) = super::wiki_export_toast(1, &ok).expect("a verdict");
        assert_eq!(de, "Wiki exportiert: 12 Dateien");

        // the singular is not "1 files"
        let one = molt_core::ExportState {
            result: "ok".to_string(),
            files: 1,
            ..molt_core::ExportState::default()
        };
        assert_eq!(
            super::wiki_export_toast(0, &one).expect("a verdict").0,
            "wiki exported: 1 file"
        );
        assert_eq!(
            super::wiki_export_toast(1, &one).expect("a verdict").0,
            "Wiki exportiert: 1 Datei"
        );

        // a failure is surfaced verbatim, in the error tone
        let bad = molt_core::ExportState {
            result: "error: dest is not a directory".to_string(),
            ..molt_core::ExportState::default()
        };
        let (msg, failed) = super::wiki_export_toast(0, &bad).expect("a verdict");
        assert!(failed, "a failure toasts in the error tone");
        assert!(
            msg.contains("dest is not a directory"),
            "the real reason survives: {msg}"
        );
    }

    /// The same outcome must toast ONCE: `apply_session` runs on every
    /// engine change, and a settled export state stays settled.
    #[test]
    fn a_settled_wiki_export_toasts_once_not_on_every_push() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless window");
        let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
        let sv = SessionView {
            wiki_export: molt_core::ExportState {
                result: "ok".to_string(),
                dest: "/tmp/out".to_string(),
                files: 3,
                bytes: 90,
                ..molt_core::ExportState::default()
            },
            ..SessionView::default()
        };
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!(ui.get_toast_text().as_str(), "wiki exported: 3 files");

        // a second, unchanged push must not speak again
        ui.invoke_show_toast("something else".into());
        apply_session(&ui, &sv, true, &chat_ui);
        assert_eq!(
            ui.get_toast_text().as_str(),
            "something else",
            "an unchanged export state re-toasted"
        );
    }

    /// **The dialog's Confirm reaches the engine with what the user picked.**
    /// Both halves are pinned: the destination (the tree lands exactly
    /// there) and the proof flag (this workspace has no chain, so a
    /// `proof: true` export is REFUSED - if the flag were dropped the very
    /// same call would write a tree).
    #[test]
    fn the_export_dialog_issues_the_command_with_the_picked_path_and_the_proof_flag() {
        i_slint_backend_testing::init_no_event_loop();
        let tmp = tempfile::tempdir().expect("tmp");
        let rt = rt();
        let _guard = rt.enter();

        // a single-operator group: propose + one approval applies the patch
        let w = molt_engine::spawn(
            GroupConfig {
                member: "me".to_string(),
                members: vec!["me".to_string()],
                threshold: 1,
                self_cosign: false,
            },
            SessionView::default(),
        );
        rt.block_on(async {
            let id = match w
                .execute(Command::Propose {
                    surface: Surface::Memory,
                    payload: serde_json::json!({
                        "op": "wiki_patch",
                        "summary": "a.md",
                        "value": "diff --git a/a.md b/a.md\nnew file mode 100644\n--- /dev/null\n+++ b/a.md\n@@ -0,0 +1,1 @@\n+hello\n",
                    }),
                })
                .await
                .expect("propose")
            {
                Reply::Proposed { id } => id,
                other => panic!("unexpected: {other:?}"),
            };
            w.execute(Command::Approve { proposal: id })
                .await
                .expect("approve");
        });

        let ui = AppWindow::new().expect("headless window");
        let cx = Ctx {
            rt: rt.handle().clone(),
            wallet: w.clone(),
            weak: ui.as_weak(),
            last_settings: Arc::new(Mutex::new(None)),
            chat_ui: Arc::new(Mutex::new(ChatUiState::default())),
        };
        wire_wiki_export(&ui, &cx);

        // --- the proof flag: no chain here, so the engine must refuse
        let refused = tmp.path().join("refused");
        ui.invoke_wiki_export(refused.display().to_string().into(), true);

        // --- the destination: the same call without the bundle writes
        let dest = tmp.path().join("out");
        ui.invoke_wiki_export(dest.display().to_string().into(), false);
        rt.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let Ok(Reply::Session(s)) = w.execute(Command::ReadSession).await else {
                    panic!("read session");
                };
                if !s.wiki_export.running && !s.wiki_export.result.is_empty() {
                    assert_eq!(s.wiki_export.result, "ok", "the export failed: {s:?}");
                    assert_eq!(s.wiki_export.dest, dest.display().to_string());
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the export never settled: {:?}",
                    s.wiki_export
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });
        assert_eq!(
            std::fs::read_to_string(dest.join("wiki/a.md")).expect("the exported doc"),
            "hello\n"
        );
        assert!(
            !dest.join("proof").exists(),
            "proof: false must write no bundle"
        );
        // the refused call ran first and left nothing behind
        assert!(
            !refused.exists(),
            "a proof export without a chain must be refused, not written"
        );
    }

    /// i18n: every wiki-export string carries a real English AND a real
    /// German arm (an empty or identical pair is a missing translation),
    /// and none of them smuggles in an em dash.
    #[test]
    fn every_wiki_export_string_reads_in_both_languages() {
        let en = Lexicon::en();
        let de = Lexicon::de();
        let pairs = [
            ("mem_tb_export", en.mem_tb_export, de.mem_tb_export),
            ("mem_ex_title", en.mem_ex_title, de.mem_ex_title),
            ("mem_ex_body", en.mem_ex_body, de.mem_ex_body),
            ("mem_ex_confirm", en.mem_ex_confirm, de.mem_ex_confirm),
            ("mem_ex_proof", en.mem_ex_proof, de.mem_ex_proof),
            ("mem_ex_reveals", en.mem_ex_reveals, de.mem_ex_reveals),
            ("mem_ex_drafts", en.mem_ex_drafts, de.mem_ex_drafts),
            ("mem_ex_done", en.mem_ex_done, de.mem_ex_done),
            ("mem_ex_file", en.mem_ex_file, de.mem_ex_file),
            ("mem_ex_files", en.mem_ex_files, de.mem_ex_files),
            ("mem_ex_failed", en.mem_ex_failed, de.mem_ex_failed),
        ];
        for (key, e, d) in pairs {
            assert!(!e.is_empty() && !d.is_empty(), "{key}: an empty arm");
            assert_ne!(e, d, "{key}: untranslated");
            assert!(!e.contains('—') && !d.contains('—'), "{key}: em dash");
        }
        // the disclosure names what the bundle actually reveals
        for l in [en, de] {
            let line = l.mem_ex_reveals.to_lowercase();
            for token in ["relay", "chart"] {
                assert!(
                    line.contains(token),
                    "the disclosure drops {token}: {}",
                    l.mem_ex_reveals
                );
            }
        }
    }

    /// The engine's export refusals reach the user in German too - the
    /// `localize_error` match carries no wildcard, so a new phrase is a
    /// compile-time reminder, but a phrase without an arm would silently
    /// stay English.
    #[test]
    fn the_wiki_export_refusals_render_in_german() {
        for phrase in [
            "a target directory is required",
            "an export is already running",
            "the wiki is empty",
            "proof needs chain governance",
            "proof needs the genesis block",
        ] {
            let e = molt_core::MoltError::WikiExport(phrase);
            let de = super::localize_error(1, &e);
            assert!(de.starts_with("Wiki-Export: "), "{de}");
            assert!(!de.contains(phrase), "phrase without a German arm: {de}");
        }
    }
}
