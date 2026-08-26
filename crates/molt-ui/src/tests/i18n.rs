// SPDX-License-Identifier: GPL-3.0-or-later
//! Localization coverage: the German tables are set-equal with the engine's
//! inventories, every rendering keeps its slots, unknown text passes through.

use super::*;

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
    let src = include_str!("../wiki.rs");
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

/// **The `Strings`/`lexicon!` pairing is guarded in ONE direction
/// only**: an entry whose field has no property fails to compile, but
/// a property with no entry compiles and renders as an EMPTY string in
/// both languages. This scans the two sources against each other, so a
/// forgotten pair goes red here instead of shipping a blank label.
#[test]
fn every_strings_property_has_an_english_and_a_german_arm() {
    let theme = include_str!("../../../molt-ui-window/ui/theme.slint");
    let lex = include_str!("../i18n.rs");
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
