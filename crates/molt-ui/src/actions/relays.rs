// SPDX-License-Identifier: GPL-3.0-or-later
//! Relay-pool callbacks: the Settings pool (add / remove / move / confirm /
//! revoke / session clearnet unlock), adopting an invite's relays, and the
//! R6 pool-edit modal's draft rows. URLs validate through molt-core's own
//! parser so the field message and the engine gate never disagree.

use molt_core::relay::RelayUrlError;
use molt_core::Command;
use slint::{ComponentHandle, Model};

use crate::i18n::Lexicon;
use crate::models::sync_strings;
use crate::app::Ctx;
use crate::{AppWindow};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    {
        // Add a relay to the pool. The URL is pre-validated with molt-core's
        // own parser so the message under the field is localized; the engine
        // re-validates and stays the gate. The draft is cleared only once the
        // engine actually accepted the entry.
        let cx = ctx.clone();
        ui.on_relay_add(move |url| {
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            let pool: Vec<String> = ui
                .get_relay_rows()
                .iter()
                .map(|r| r.url.to_string())
                .collect();
            if let Err(msg) = relay_add_check(ui.get_lang_index(), url.as_str(), &pool) {
                ui.set_relay_error(msg.into());
                return;
            }
            ui.set_relay_error("".into());
            let w = cx.wallet.clone();
            let weak = ui.as_weak();
            cx.rt.spawn(async move {
                let res = w
                    .execute(Command::RelayAdd {
                        url: url.to_string(),
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else {
                        return;
                    };
                    match res {
                        Ok(_) => {
                            ui.set_relay_draft("".into());
                            ui.set_relay_error("".into());
                        }
                        // an engine refusal the local check did not foresee
                        // (a concurrent MCP edit, a future rule) belongs
                        // under the field verbatim, never nowhere
                        Err(e) => ui.set_relay_error(e.to_string().into()),
                    }
                });
            });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_relay_remove(move |url| {
            cx.issue(
                Command::RelayRemove {
                    url: url.to_string(),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_relay_move(move |url, up| {
            cx.issue(
                Command::RelayMove {
                    url: url.to_string(),
                    up,
                },
            );
        });
    }

    {
        // `accept_clearnet` rides in from the GUI's warning dialog — the
        // engine enforces it either way, so an MCP agent faces the same gate.
        let cx = ctx.clone();
        ui.on_relay_confirm(move |url, accept_clearnet| {
            cx.issue(
                Command::RelayConfirm {
                    url: url.to_string(),
                    accept_clearnet,
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_relay_revoke(move |url| {
            cx.issue(
                Command::RelayRevoke {
                    url: url.to_string(),
                },
            );
        });
    }

    {
        // Session-only clearnet activation: never persisted, so a restart
        // re-arms the gate by itself.
        let cx = ctx.clone();
        ui.on_relay_clearnet_session(move |unlock| {
            cx.issue(Command::RelayClearnetSession { unlock });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_adopt_invite_relays(move |link| {
            let Ok(inv) = molt_engine::FoundingInvite::parse(&link) else {
                return;
            };
            for url in inv.handover.relays {
                cx.issue(Command::RelayAdd { url: url.clone() });
                // an ONION relay needs no exposure decision — confirm it
                // outright. A clearnet one keeps its acknowledgement: making
                // the convenient path the less private one is exactly what
                // this button must not do.
                if molt_core::relay::relay_kind(&url) == molt_core::relay::RelayKind::Onion {
                    cx.issue(
                        Command::RelayConfirm { url, accept_clearnet: false },
                    );
                }
            }
        });
    }

    // R6 pool-edit modal: the draft is a row table. Seed copies the
    // effective pool, add validates through molt-core's own parser (the
    // same gate the engine applies — and against the DRAFT, so a queued
    // duplicate is caught too), remove drops a row. The space-joined
    // `org-relays-draft` string stays the org-propose payload, so the
    // propose path below is untouched.
    {
        let weak = ui.as_weak();
        ui.on_relays_draft_seed(move || {
            let Some(ui) = weak.upgrade() else { return };
            let rows: Vec<String> = ui.get_org_relays().iter().map(|s| s.to_string()).collect();
            set_relay_draft_rows(&ui, &rows);
            ui.set_org_relay_add_draft("".into());
            ui.set_org_relay_add_error("".into());
        });
        let weak = ui.as_weak();
        ui.on_relays_draft_add(move |url| {
            let Some(ui) = weak.upgrade() else { return };
            let mut rows: Vec<String> = ui
                .get_org_relays_draft_rows()
                .iter()
                .map(|s| s.to_string())
                .collect();
            match relay_add_check(ui.get_lang_index(), url.as_str(), &rows) {
                Err(msg) => ui.set_org_relay_add_error(msg.into()),
                Ok(canon) => {
                    rows.push(canon);
                    set_relay_draft_rows(&ui, &rows);
                    ui.set_org_relay_add_draft("".into());
                    ui.set_org_relay_add_error("".into());
                }
            }
        });
        let weak = ui.as_weak();
        ui.on_relays_draft_remove(move |i| {
            let Some(ui) = weak.upgrade() else { return };
            let mut rows: Vec<String> = ui
                .get_org_relays_draft_rows()
                .iter()
                .map(|s| s.to_string())
                .collect();
            let Ok(i) = usize::try_from(i) else { return };
            if i < rows.len() {
                rows.remove(i);
                set_relay_draft_rows(&ui, &rows);
                // a removed row may end the condition an add error named
                ui.set_org_relay_add_error("".into());
            }
        });
    }
}

/// The R6 pool-edit modal's one draft setter: rows are the editing truth,
/// the space-joined string is the org-propose payload — set together so the
/// two can never disagree. The overlap flag mirrors the engine's
/// make-before-break gate into `confirm-enabled`, so the modal never sends
/// a draft the engine would refuse after the rows are already gone.
fn set_relay_draft_rows(ui: &AppWindow, rows: &[String]) {
    let current: Vec<String> = ui.get_org_relays().iter().map(|s| s.to_string()).collect();
    let overlap = current.is_empty() || rows.iter().any(|r| current.contains(r));
    ui.set_org_relays_draft_overlap(overlap);
    ui.set_org_relays_draft(rows.join(" ").into());
    sync_strings(&ui.get_org_relays_draft_rows(), rows, |m| {
        ui.set_org_relays_draft_rows(m)
    });
}

/// Validate a pool-add URL: `Ok` carries the CANONICAL spelling to store,
/// `Err` the localized reason the pool refuses it. Validation runs through
/// molt-core's OWN parser (the very function the engine gates on, so the
/// field message and the gate can never disagree); the engine still
/// re-validates and stays the authority.
pub(crate) fn relay_add_check(lang: i32, raw: &str, pool: &[String]) -> Result<String, &'static str> {
    let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
    match molt_core::relay::normalize_relay_url(raw) {
        Err(RelayUrlError::Scheme) => Err(l.rp_err_scheme),
        Err(RelayUrlError::Host) => Err(l.rp_err_host),
        Err(RelayUrlError::PlaintextClearnet) => Err(l.rp_err_plain),
        Err(RelayUrlError::Junk) => Err(l.rp_err_junk),
        Err(RelayUrlError::OnionAddress) => Err(l.rp_err_onion),
        Err(RelayUrlError::Userinfo) => Err(l.rp_err_userinfo),
        Err(RelayUrlError::Fragment) => Err(l.rp_err_fragment),
        Err(RelayUrlError::TooLong) => Err(l.rp_err_toolong),
        Err(RelayUrlError::NonCanonical) => Err(l.rp_err_noncanon),
        Ok(url) if pool.contains(&url) => Err(l.rp_err_dup),
        Ok(url) => Ok(url),
    }
}
