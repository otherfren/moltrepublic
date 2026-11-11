// SPDX-License-Identifier: GPL-3.0-or-later

//! Picture proposals at the propose door: the default name, the `path`
//! source and the downscale (`beta_feedback_2026-11-11.md` WP6).

use super::support::*;
use crate::image_fit::tests::noisy;
use crate::*;
use base64::Engine as _;
use serde_json::json;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn propose(w: &WalletHandle, payload: serde_json::Value) -> Result<Reply, MoltError> {
    w.execute(Command::Propose { surface: Surface::Organization, payload }).await
}

/// The newest pending Organization card's payload.
async fn pending_payload(w: &WalletHandle) -> serde_json::Value {
    let pending = read_surface(w, Surface::Organization).await.pending;
    pending.iter().max_by_key(|p| p.id.0).expect("a pending card").payload.clone()
}

fn carried(payload: &serde_json::Value) -> Vec<u8> {
    crate::proposals::image_bytes(payload).expect("the card carries bytes")
}

#[test]
fn an_unnamed_picture_gets_a_name_from_its_own_format() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let jpeg = noisy(8, 8, image::ImageFormat::Jpeg);
        let png = noisy(8, 8, image::ImageFormat::Png);
        propose(&w, json!({ "op": "set_image", "bytes_b64": b64(&jpeg) })).await.expect("propose");
        assert_eq!(pending_payload(&w).await["value"], "logo.jpg");
        propose(&w, json!({ "op": "set_image", "value": "  ", "bytes_b64": b64(&png) }))
            .await
            .expect("propose");
        assert_eq!(pending_payload(&w).await["value"], "logo.png");
        propose(&w, json!({ "op": "set_member_image", "member": "me", "bytes_b64": b64(&png) }))
            .await
            .expect("propose");
        assert_eq!(pending_payload(&w).await["value"], "avatar.png");
        // a given name stays
        propose(&w, json!({ "op": "set_image", "value": "crest.png", "bytes_b64": b64(&jpeg) }))
            .await
            .expect("propose");
        assert_eq!(pending_payload(&w).await["value"], "crest.png");
    });
}

#[test]
fn an_oversized_picture_is_downscaled_and_the_card_carries_the_small_one() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let big = noisy(700, 350, image::ImageFormat::Png);
        assert!(big.len() > 256 * 1024, "the fixture must be over any budget: {}", big.len());
        propose(&w, json!({ "op": "set_image", "value": "big.png", "bytes_b64": b64(&big) }))
            .await
            .expect("an oversized picture is fitted, not refused");
        let payload = pending_payload(&w).await;
        let small = carried(&payload);
        assert!(small.len() < big.len());
        assert_eq!(image::guess_format(&small).expect("sniffs"), image::ImageFormat::Png);
        assert!(crate::proposals::payload_fits(
            Surface::Organization,
            &payload,
            &["me".to_string(), "peer-1".to_string(), "peer-2".to_string()],
        ));
    });
}

#[test]
fn a_path_is_read_by_the_node_for_logo_and_avatar() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let dir = tempfile::tempdir().expect("tempdir");
        let small = noisy(16, 16, image::ImageFormat::Png);
        let logo = dir.path().join("crest.png");
        std::fs::write(&logo, &small).expect("write");
        propose(&w, json!({ "op": "set_image", "path": logo.display().to_string() }))
            .await
            .expect("propose by path");
        let payload = pending_payload(&w).await;
        assert_eq!(payload["value"], "crest.png");
        assert_eq!(carried(&payload), small, "a fitting file travels untouched");
        assert!(payload.get("path").is_none(), "the host path is never signed");

        // an oversized file is fitted on the way in
        let big = dir.path().join("big.bmp");
        std::fs::write(&big, noisy(600, 300, image::ImageFormat::Bmp)).expect("write");
        propose(&w, json!({ "op": "set_image", "path": big.display().to_string() }))
            .await
            .expect("propose by path");
        let fitted = carried(&pending_payload(&w).await);
        assert_eq!(image::guess_format(&fitted).expect("sniffs"), image::ImageFormat::Bmp);

        propose(&w, json!({ "op": "set_member_image", "member": "me", "path": logo.display().to_string() }))
            .await
            .expect("avatar by path");
        let payload = pending_payload(&w).await;
        assert_eq!(payload["op"], "set_member_image");
        assert_eq!(carried(&payload), small);
        assert!(payload.get("path").is_none());
    });
}

#[test]
fn path_and_bytes_are_alternatives() {
    rt().block_on(async {
        let w = spawn(GroupConfig::demo(), SessionView::default());
        let dir = tempfile::tempdir().expect("tempdir");
        let png = noisy(16, 16, image::ImageFormat::Png);
        let file = dir.path().join("a.png");
        std::fs::write(&file, &png).expect("write");
        let path = file.display().to_string();
        let msg = |r: Result<Reply, MoltError>| match r {
            Err(MoltError::BadPayload(m)) => m,
            other => panic!("expected a refusal: {other:?}"),
        };
        for op in ["set_image", "set_member_image"] {
            let both = msg(propose(&w, json!({ "op": op, "member": "me", "path": path, "bytes_b64": b64(&png) })).await);
            assert!(both.contains("`path`") && both.contains("`bytes_b64`"), "{both}");
            let neither = msg(propose(&w, json!({ "op": op, "member": "me", "value": "a.png" })).await);
            assert!(neither.contains("`path`") && neither.contains("`bytes_b64`"), "{neither}");
        }
        let gone = dir.path().join("none.png").display().to_string();
        let gone = msg(propose(&w, json!({ "op": "set_image", "path": gone })).await);
        assert!(gone.contains("cannot read"), "{gone}");
        let relative = msg(propose(&w, json!({ "op": "set_image", "path": "pics/a.png" })).await);
        assert!(relative.contains("absolute"), "{relative}");
        assert!(read_surface(&w, Surface::Organization).await.pending.is_empty());
    });
}
