// SPDX-License-Identifier: GPL-3.0-or-later
//! Organization and governance callbacks: the Members / Uploads table
//! sort, filter and pager, the vote buttons (propose / approve / decline /
//! withdraw), the org edit modals (charter, logo, member profile - the
//! picked picture rides the proposal as bytes, sign-what-you-see) and the
//! proposed-image viewer.

use molt_core::{Command, ProposalId, Surface};
use slint::ComponentHandle;

use crate::i18n::error_toast;
use crate::images::{fit_member_image, image_from_bytes, proposal_image_from_b64, ImageFitError};
use crate::labels::default_op;
use crate::app::Ctx;
use crate::{AppWindow, Strings};

pub(crate) fn wire(ui: &AppWindow, ctx: &Ctx) {
    // The Members/Uploads tables' sort/filter. View-local presentation like
    // the Open/backup lists — but these mirrored rows are rebuilt from the
    // engine on every push, so the state lives in ChatUiState (toggle in
    // Rust, single writer) and push_surfaces re-applies it each time; the
    // engine's ReadMembers/ReadUploads stay the full projections for MCP.
    {
        let cx = ctx.clone();
        ui.on_sort_members(move |column| {
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.sort_members_by(column.as_str());
            }
            cx.refresh_surfaces();
        });
    }

    {
        let cx = ctx.clone();
        ui.on_sort_uploads(move |column| {
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.sort_uploads_by(column.as_str());
            }
            cx.refresh_surfaces();
        });
    }

    {
        let cx = ctx.clone();
        ui.on_filter_uploads(move |needle| {
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.set_uploads_filter(needle.to_string());
            }
            cx.refresh_surfaces();
        });
    }

    {
        // The proposal-outcome lists' pager (Organization → Declined, the
        // gated surfaces' applied log): step the UI-local page, then
        // re-push — the push clamps against the list's current length and
        // echoes "page x of y" back into the surface tab.
        let cx = ctx.clone();
        ui.on_page_list(move |surface, list, delta| {
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.page_list_by(surface.as_str(), list.as_str(), delta);
            }
            cx.refresh_surfaces();
        });
    }

    {
        // the two Shared Files votes: the engine fills a persist's identity
        // and refuses what the tables cannot take; an unpersist carries the
        // stamp its fresh window starts from
        let cx = ctx.clone();
        ui.on_persist_upload(move |id| {
            cx.issue(Command::Propose {
                surface: Surface::Files,
                payload: serde_json::json!({ "op": "persist", "id": id.as_str() }),
            });
        });
        let cx = ctx.clone();
        ui.on_unpersist_upload(move |id| {
            cx.issue(Command::Propose {
                surface: Surface::Files,
                payload: serde_json::json!({
                    "op": "unpersist",
                    "id": id.as_str(),
                    "at": crate::labels::unix_now(),
                }),
            });
        });
    }

    {
        // the mirror switch and quota (mirroring §3.6): one command, the
        // engine declares it to the members; a field that is not a number
        // keeps the stored quota
        let cx = ctx.clone();
        ui.on_set_mirror(move |on, quota_text| {
            let stored = cx
                .weak
                .upgrade()
                .map(|ui| ui.get_org_mirror_quota().to_string())
                .unwrap_or_default();
            let quota_bytes = crate::labels::gb_bytes(quota_text.as_str())
                .or_else(|| crate::labels::gb_bytes(&stored))
                .unwrap_or(molt_core::MIRROR_QUOTA_DEFAULT);
            cx.issue(Command::SetMirror { on, quota_bytes });
        });
        // the mirror folder: any-path, so the picker runs here and the
        // engine gets the choice through its GUI-only door
        let cx = ctx.clone();
        ui.on_pick_mirror_dir(move || {
            let cx2 = cx.clone();
            let start = cx
                .weak
                .upgrade()
                .map(|ui| ui.get_org_mirror_dir().to_string())
                .unwrap_or_default();
            cx.rt.spawn(async move {
                let mut picker = rfd::AsyncFileDialog::new();
                if !start.is_empty() {
                    picker = picker.set_directory(start);
                }
                let Some(folder) = picker.pick_folder().await else {
                    return; // cancelled
                };
                cx2.issue(Command::SetMirrorDir {
                    path: folder.path().display().to_string(),
                });
            });
        });
    }

    {
        // A member row's uploads count: jump to Shared Files → Uploads
        // pre-filtered to that member. The view switch is the same engine
        // command the nav issues; the filter itself stays single-writer in
        // ChatUiState and the push echoes it into the filter box.
        let cx = ctx.clone();
        ui.on_jump_member_uploads(move |member| {
            if let Ok(mut st) = cx.chat_ui.lock() {
                st.set_uploads_filter(member.to_string());
            }
            cx.issue(
                Command::SelectView {
                    surface: Surface::Files,
                    view: "uploads".to_string(),
                },
            );
            cx.refresh_surfaces();
        });
    }

    {
        let cx = ctx.clone();
        ui.on_propose(move |key, title| {
            let title = title.trim().to_string();
            if title.is_empty() {
                return;
            }
            let Some(surface) = Surface::parse(&key) else {
                return;
            };
            if !surface.is_gated() {
                return;
            }
            let payload = serde_json::json!({ "op": default_op(surface), "title": title });
            cx.issue(Command::Propose { surface, payload });
        });
    }

    // an Organization change from the status screen's edit modals (charter /
    // image): the same Command::Propose the MCP propose tool drives — the
    // drafted value rides along under "value", the display title under
    // "title" (what the pending cards summarize). A set_image reads the
    // picked file OFF the UI thread and embeds the bytes as base64
    // (sign-what-you-see: members vote on the actual image; the engine
    // refuses anything over its cap with an honest error toast).
    {
        let cx = ctx.clone();
        ui.on_org_propose(move |op, value| {
            if op.as_str() == "set_image" {
                let w = cx.wallet.clone();
                let weak = cx.weak.clone();
                let path = value.to_string();
                cx.rt.spawn(async move {
                    let read = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || std::fs::read(&path)
                    })
                    .await;
                    let payload = match read {
                        Ok(Ok(bytes)) => {
                            use base64::Engine as _;
                            // WP3 pre-check with the REAL preview decoder:
                            // instant, localized feedback instead of an
                            // engine-error round-trip. The engine's co-equal
                            // sniff (molt-engine proposals.rs
                            // `image_decodable`) still guards the command
                            // path for every frontend — deliberate
                            // duplication, each side references the other.
                            if image_from_bytes(&bytes).is_none() {
                                let weak = weak.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = weak.upgrade() {
                                        let msg =
                                            ui.global::<Strings>().get_pc_img_missing();
                                        ui.invoke_show_toast(msg);
                                    }
                                });
                                return;
                            }
                            let name = std::path::Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.clone());
                            // no baked display title: the op is the
                            // language-neutral placeholder every UI
                            // translates at render time (display_title)
                            serde_json::json!({
                                "op": "set_image",
                                "value": name,
                                "bytes_b64":
                                    base64::engine::general_purpose::STANDARD.encode(bytes),
                            })
                        }
                        _ => {
                            // no Debug dump at the user: the one important
                            // thing is WHICH file did not read
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    let msg = format!(
                                        "\u{26a0} {} {path}",
                                        ui.global::<Strings>().get_toast_file_unreadable()
                                    );
                                    ui.invoke_show_toast_error(msg.into());
                                }
                            });
                            return;
                        }
                    };
                    // the confirmation belongs to the OUTCOME: this path
                    // can still fail on the engine's own decode sniff or
                    // the payload cap, and a "Proposed" toast on the click
                    // followed by an error described a proposal that never
                    // existed
                    let outcome = w
                        .execute(Command::Propose {
                            surface: Surface::Organization,
                            payload,
                        })
                        .await;
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = weak.upgrade() else { return };
                        match outcome {
                            Ok(_) => {
                                let msg = ui.global::<Strings>().get_toast_proposed();
                                ui.invoke_show_toast(msg);
                            }
                            Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
                        }
                    });
                });
                return;
            }
            let payload = serde_json::json!({
                "op": op.as_str(),
                "value": value.as_str(),
            });
            let msg = cx
                .weak
                .upgrade()
                .map(|ui| ui.global::<Strings>().get_toast_proposed().to_string())
                .unwrap_or_default();
            cx.issue_then_toast(
                Command::Propose {
                    surface: Surface::Organization,
                    payload,
                },
                msg,
            );
        });
    }

    // Organization → Members: the OWN seat's profile. `set_member_image`
    // reads the picked file OFF the UI thread, fits it to what this
    // republic still carries (square + budget) and embeds the bytes;
    // everything else is a plain payload. The engine refuses a profile op
    // proposed for another seat, so `member` is always the own one.
    {
        let cx = ctx.clone();
        ui.on_member_propose(move |op, member, value| {
            if op.as_str() != "set_member_image" {
                // a removal carries no value at all - the payload is what
                // the members sign, so it says only what it changes
                let payload = if op.as_str() == "remove_member_image" {
                    serde_json::json!({ "op": op.as_str(), "member": member.as_str() })
                } else {
                    serde_json::json!({
                        "op": op.as_str(),
                        "member": member.as_str(),
                        "value": value.as_str(),
                    })
                };
                let msg = cx
                    .weak
                    .upgrade()
                    .map(|ui| ui.global::<Strings>().get_toast_proposed().to_string())
                    .unwrap_or_default();
                cx.issue_then_toast(
                    Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    },
                    msg,
                );
                return;
            }
            let budget = cx
                .weak
                .upgrade()
                .map(|ui| usize::try_from(ui.get_mp_img_budget()).unwrap_or(0))
                .unwrap_or(0);
            let w = cx.wallet.clone();
            let weak = cx.weak.clone();
            let member = member.to_string();
            let path = value.to_string();
            cx.rt.spawn(async move {
                let read = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || std::fs::read(&path)
                })
                .await;
                let Ok(Ok(bytes)) = read else {
                    // no Debug dump at the user: the one important thing is
                    // WHICH file did not read
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            let msg = format!(
                                "\u{26a0} {} {path}",
                                ui.global::<Strings>().get_toast_file_unreadable()
                            );
                            ui.invoke_show_toast_error(msg.into());
                        }
                    });
                    return;
                };
                // the crop/downscale is CPU work on a picture up to 8192²
                let fitted =
                    tokio::task::spawn_blocking(move || fit_member_image(&bytes, budget)).await;
                let fitted = match fitted {
                    Ok(Ok(fitted)) => fitted,
                    Ok(Err(why)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                let s = ui.global::<Strings>();
                                ui.invoke_show_toast_error(match why {
                                    ImageFitError::Undecodable => s.get_pc_img_missing(),
                                    ImageFitError::TooLarge => s.get_mp_img_too_big(),
                                });
                            }
                        });
                        return;
                    }
                    Err(_) => return,
                };
                use base64::Engine as _;
                // the name must match the bytes: the engine derives the
                // avatar file's extension from this display value
                let stem = std::path::Path::new(&path)
                    .file_stem()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| member.clone());
                let payload = serde_json::json!({
                    "op": "set_member_image",
                    "member": member,
                    "value": format!("{stem}.{}", fitted.ext),
                    "bytes_b64":
                        base64::engine::general_purpose::STANDARD.encode(fitted.bytes),
                });
                // the confirmation belongs to the OUTCOME: the engine's own
                // gates (square, budget, the seat) still run after this
                let outcome = w
                    .execute(Command::Propose {
                        surface: Surface::Organization,
                        payload,
                    })
                    .await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match outcome {
                        Ok(_) => {
                            let msg = ui.global::<Strings>().get_toast_proposed();
                            ui.invoke_show_toast(msg);
                        }
                        Err(e) => ui.invoke_show_toast_error(error_toast(&ui, &e)),
                    }
                });
            });
        });
    }

    // pick the own seat's picture — same picker set as the republic image
    {
        let cx = ctx.clone();
        ui.on_mp_img_pick(move || {
            let weak = cx.weak.clone();
            cx.rt.spawn(async move {
                let picker = rfd::AsyncFileDialog::new()
                    // no "svg": the engine refuses it, and a square check
                    // on a vector is meaningless
                    .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif", "bmp"]);
                let Some(file) = picker.pick_file().await else {
                    return; // cancelled
                };
                let path = file.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_mp_img_draft(path.into());
                    }
                });
            });
        });
    }

    // pick a new republic image via the native file dialog (async XDG
    // portal, like the chat share picker) — only the path lands in the
    // draft; proposing it ships the file REFERENCE, never bytes
    {
        let cx = ctx.clone();
        ui.on_org_logo_pick(move || {
            let weak = cx.weak.clone();
            cx.rt.spawn(async move {
                let picker = rfd::AsyncFileDialog::new()
                    // no "svg" (L1, 2026-08-16): the engine refuses SVG proposals —
                    // offering a format the vote will bounce is a trap
                    .add_filter("Image", &["png", "jpg", "jpeg", "webp", "gif", "bmp"]);
                let Some(file) = picker.pick_file().await else {
                    return; // cancelled
                };
                let path = file.path().display().to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_org_logo_draft(path.into());
                    }
                });
            });
        });
    }

    // the proposed image behind a pending set_image: the bytes RODE the
    // proposal payload (sign-what-you-see), so the viewer decodes them
    // locally on every member's device — no transfer, no proposer needed.
    // Shown INLINE in the proposal's card; the same id toggles it off.
    {
        let weak = ui.as_weak();
        ui.on_view_proposal_image(move |id, img_b64| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_img_inline_id() == id {
                ui.set_img_inline_id(-1);
                return;
            }
            match proposal_image_from_b64(img_b64.as_str()) {
                Some(img) => {
                    ui.set_img_inline_src(img);
                    ui.set_img_inline_id(id);
                }
                None => {
                    let s = ui.global::<Strings>();
                    ui.invoke_show_toast_error(s.get_pc_img_missing());
                }
            }
        });
    }

    // save the ORIGINAL proposed-image bytes (no re-encode) wherever the
    // save dialog points; the suggested name is the proposal's file-name
    // value. Local bytes → the write happens right here, no engine hop.
    {
        let cx = ctx.clone();
        ui.on_save_proposal_image(move |img_b64, name| {
            use base64::Engine as _;
            let Some(ui) = cx.weak.upgrade() else {
                return;
            };
            // an empty/absent payload decodes to zero bytes — that is a
            // missing image, not a file worth a save dialog (a minimal
            // MCP proposal may carry no bytes_b64 at all)
            let bytes = match base64::engine::general_purpose::STANDARD.decode(img_b64.as_str()) {
                Ok(b) if !b.is_empty() => b,
                _ => {
                    let s = ui.global::<Strings>();
                    ui.invoke_show_toast_error(s.get_pc_img_missing());
                    return;
                }
            };
            let saved_prefix = ui.global::<Strings>().get_toast_dl_done();
            let weak = cx.weak.clone();
            cx.rt.spawn(async move {
                let Some(dest) = rfd::AsyncFileDialog::new()
                    .set_file_name(name.as_str())
                    .save_file()
                    .await
                else {
                    return; // cancelled
                };
                let path = dest.path().to_path_buf();
                let write = tokio::task::spawn_blocking(move || {
                    std::fs::write(&path, &bytes).map(|()| path)
                })
                .await;
                let msg = match write {
                    Ok(Ok(path)) => (format!("{saved_prefix} {}", path.display()), true),
                    Ok(Err(e)) => (format!("⚠ {e}"), false),
                    Err(e) => (format!("⚠ {e}"), false),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let (msg, ok) = msg;
                        if ok {
                            ui.invoke_show_toast(msg.into());
                        } else {
                            ui.invoke_show_toast_error(msg.into());
                        }
                    }
                });
            });
        });
    }

    {
        let cx = ctx.clone();
        ui.on_approve(move |id| {
            cx.issue(
                Command::Approve {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_decline(move |id| {
            cx.issue(
                Command::Decline {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }

    {
        let cx = ctx.clone();
        ui.on_withdraw(move |id| {
            cx.issue(
                Command::Withdraw {
                    proposal: ProposalId(id as u64),
                },
            );
        });
    }
}
