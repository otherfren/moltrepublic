// SPDX-License-Identifier: GPL-3.0-or-later
//! Localization (E2-E6): the `Strings` table for the Slint side, the
//! German renderings of engine-authored phrases (errors, headlines, run-log
//! shapes, transport/S3/Tor verdicts), and the honest English fallback for
//! anything unknown. Engine strings stay the stable keys.

use slint::ComponentHandle;

use crate::{AppWindow, Strings};

/// E3 (`i18n_error_codes_plan.md`): the wizard headline, localized BY
/// PHRASE — the engine's English phrase is the stable key (one inventory,
/// `molt_engine::known_headlines`, pinned producible engine-side). An
/// unknown phrase renders as itself: honest English fallback instead of
/// silence, and the coverage test below keeps the map complete.
pub(crate) fn localize_headline(lang: i32, phrase: &str) -> String {
    if lang != 1 || phrase.is_empty() {
        return phrase.to_string();
    }
    match phrase {
        "No shared relay" => "Kein gemeinsames Relay",
        "Clearnet dialing is off" => "Clearnet-Verbindungen sind aus",
        "Relay check running" => "Relay-Prüfung läuft",
        "No dialable relay" => "Kein wählbares Relay",
        "Relay not answering" => "Relay antwortet nicht",
        "No relay configured" => "Kein Relay konfiguriert",
        "No relay confirmed" => "Kein Relay bestätigt",
        "Tor cannot reach the relay" => "Tor erreicht das Relay nicht",
        "No answer in time" => "Keine Antwort rechtzeitig",
        "No relay took it" => "Kein Relay hat es angenommen",
        "Invite already used" => "Einladung bereits benutzt",
        "The founder ended it" => "Der Gründer hat es beendet",
        "The founder refused it" => "Der Gründer hat es abgelehnt",
        "Workspace already exists" => "Workspace existiert bereits",
        "Cannot decrypt the backup" => "Backup lässt sich nicht entschlüsseln",
        "Chain does not verify" => "Chain verifiziert nicht",
        "Backup carries no chain" => "Backup trägt keine Chain",
        "No seat in this roster" => "Kein Sitz in diesem Roster",
        "Workspace is open" => "Workspace ist geöffnet",
        "Cannot read the file" => "Datei lässt sich nicht lesen",
        "Backup file too big" => "Backup-Datei zu groß",
        "No backup in the bucket" => "Kein Backup im Bucket",
        "Download failed" => "Download fehlgeschlagen",
        other => return other.to_string(),
    }
    .to_string()
}

/// E2 (`i18n_error_codes_plan.md`): render a [`MoltError`] in the ACTIVE
/// language. English stays the engine `Display` verbatim (MCP parity);
/// the German arms are authored here, compact, with engine-diagnostic
/// free-text tails carried through untouched. The match deliberately has
/// NO wildcard — a new variant fails compilation here until it gets a
/// German arm (the plan's coverage rule, in compiler form).
pub(crate) fn localize_error(lang: i32, e: &molt_core::MoltError) -> String {
    use molt_core::MoltError as E;
    if lang != 1 {
        return e.to_string();
    }
    match e {
        E::UnknownProposal(id) => format!("Unbekannter Vorschlag #{}", id.0),
        E::NotGated(sf) => format!("{} ist ungated - nichts vorzuschlagen", sf.as_str()),
        E::BadPayload(t) => format!("Ungültige Payload: {t}"),
        E::FeatureDisabled(k) => format!("{k}: nicht aktiviert"),
        E::AlreadyTerminal(id, st) => format!("Vorschlag #{} ist bereits {st:?}", id.0),
        E::NotTheProposer(id) => format!("Vorschlag #{}: nur wer vorschlägt, zieht zurück", id.0),
        E::AlreadyApproved(id) => format!(
            "Vorschlag #{} trägt die Zustimmung dieses Nodes bereits - die übrigen Stimmen müssen von den Mitgliedern selbst kommen",
            id.0
        ),
        E::AlreadyDeclined(id) => format!("Vorschlag #{} trägt die Ablehnung dieses Mitglieds bereits", id.0),
        E::DiscussionClosed(id, st) => format!("Diskussion zu Vorschlag #{} ist schreibgeschützt - der Vote ist {st:?}", id.0),
        E::Settings(t) => format!("Einstellungen: {t}"),
        E::UnknownWorkspace(w) => format!("Unbekannter Workspace `{w}`"),
        E::WorkspaceBusy(t) => format!("Workspace ist belegt: {t}"),
        E::WorkspaceEncrypted(w) => format!("Workspace `{w}` ist versiegelt - erst entsiegeln"),
        E::Storage(t) => format!("Speicher: {t}"),
        E::UnknownView(sf, v) => format!("Surface {sf:?} hat keine Ansicht `{v}`"),
        E::UnknownMessage(id) => format!("Unbekannte Chat-Nachricht {id}"),
        E::NoFile(id) => format!("Nachricht {id} trägt keine geteilte Datei"),
        E::FileUnavailable(id) => format!("Die geteilte Datei an {id} ist nicht mehr verfügbar"),
        E::FileExpired(id) => format!("Die geteilte Datei an {id} ist aus dem Aufbewahrungsfenster gealtert"),
        E::NotYourFile(_) => "Nur wer die Datei geteilt hat, kann sie entfernen".to_string(),
        E::NotYourMessage(_) => "Nur wer die Nachricht geschrieben hat, kann sie löschen".to_string(),
        E::Restore(t) => format!("Restore: {t}"),
        E::Create(t) => format!("Gründung: {t}"),
        E::Join(t) => format!("Beitritt: {t}"),
        E::Recover(t) => format!("Recovery: {t}"),
        E::Poke(t) => format!(
            "Anstupsen: {}",
            match *t {
                "not enabled" => "nicht aktiviert",
                "cannot poke yourself" => "nicht dich selbst",
                "unknown member" => "unbekanntes Mitglied",
                other => other,
            }
        ),
        E::WikiExport(t) => format!(
            "Wiki-Export: {}",
            match *t {
                "a target directory is required" => "Zielordner fehlt",
                "an export is already running" => "läuft bereits",
                "the wiki is empty" => "Wiki ist leer",
                // the dialog's checkbox says "Prüfpaket beilegen" — the
                // refusal has to name the same thing the user just ticked
                "proof needs chain governance" => "Prüfpaket braucht Chain-Governance",
                "proof needs the genesis block" => "Prüfpaket braucht den Genesis-Block",
                other => other,
            }
        ),
        E::Engine(t) => format!("Engine: {t}"),
    }
}

/// The one way an engine error becomes toast copy: ⚠ + the localized
/// rendering (E2). Reads the language off the window, so it must run on
/// the UI thread.
pub(crate) fn error_toast(ui: &AppWindow, e: &molt_core::MoltError) -> slint::SharedString {
    format!("⚠ {}", localize_error(ui.get_lang_index(), e)).into()
}

/// The wiki pane's own refusals (E6): GUI-authored one-liners, localized
/// by phrase with the honest-English fallback. Pinned by a source scan
/// over `wiki.rs`, so a new `Err("…")` there goes red until it has an arm.
pub(crate) fn localize_wiki_err(lang: i32, e: &str) -> &str {
    if lang != 1 {
        return e;
    }
    match e {
        "unknown folder" => "unbekannter Ordner",
        "too deep" => "zu tief verschachtelt",
        "empty name" => "leerer Name",
        "no path separators" => "keine Pfadtrenner im Namen",
        "name already taken" => "Name schon vergeben",
        "unknown file" => "unbekannte Datei",
        "deleted" => "gelöscht",
        "into itself" => "nicht in sich selbst",
        "nothing to undo" => "nichts rückgängig zu machen",
        "folder not empty" => "Ordner nicht leer",
        "a draft has no base" => "ein Entwurf hat keine Basis",
        other => other,
    }
}

/// The header pill's transport reason (E6): engine-composed parts joined
/// with "; ", each a known phrase around a member name, a count or a
/// free-text error tail — localized part-wise; unknown tails ride verbatim.
pub(crate) fn localize_net_reason(lang: i32, reason: &str) -> String {
    if lang != 1 || reason.is_empty() {
        return reason.to_string();
    }
    let part_de = |part: &str| -> String {
        if let Some((m, r)) = part.strip_prefix("link to ").and_then(|r| r.split_once(": ")) {
            return format!("Verbindung zu {m}: {}", net_phrase_de(r));
        }
        if let Some((m, r)) = part.strip_prefix("sends to ").and_then(|r| r.split_once(": ")) {
            return format!("Zustellung an {m}: {}", net_phrase_de(r));
        }
        if let Some(why) = part.strip_prefix("relays: ") {
            return format!("Relays: {}", net_phrase_de(why));
        }
        if let Some(n) = part.strip_suffix(" frames past the key ring") {
            return format!("{n} Frames jenseits des Schlüsselrings");
        }
        net_phrase_de(part)
    };
    reason.split("; ").map(part_de).collect::<Vec<_>>().join("; ")
}

/// One inner transport phrase → German. Statics match by prefix (the
/// engine composes them with line continuations); free-text NetError
/// tails fall through verbatim.
fn net_phrase_de(p: &str) -> String {
    if let Some(n) = p
        .strip_prefix("no live relay connection (0 of ")
        .and_then(|r| r.strip_suffix(" up, reconnecting)"))
    {
        return format!("keine lebende Relay-Verbindung (0 von {n} erreichbar, verbinde neu)");
    }
    if p.starts_with("no relay channel") {
        return "kein Relay-Kanal - Relays dieser Republik unter Einstellungen prüfen".into();
    }
    if p.starts_with("offline: no queue credentials") {
        return "offline: keine Queue-Credentials auf der Platte - per Recovery-Link neu \
                beitreten"
            .into();
    }
    if p.starts_with("offline: no MLS group snapshot") {
        return "offline: kein MLS-Gruppen-Snapshot auf der Platte - per Recovery-Link neu \
                beitreten"
            .into();
    }
    if p.starts_with("offline: no mesh links") {
        return "offline: keine Mesh-Links auf der Platte - per Recovery-Link neu beitreten"
            .into();
    }
    if p.starts_with("offline: resuming the persisted mesh") {
        return "offline: das persistierte Mesh ließ sich nicht fortsetzen - nichts \
                erreicht die Peers"
            .into();
    }
    match p {
        "connecting" => "verbinde".into(),
        "inbound subscription ended - resubscribing" => {
            "Eingangs-Subscription endete - erneuere".into()
        }
        "deliveries keep going unacknowledged - still resending" => {
            "Zustellungen bleiben unbestätigt - sende weiter".into()
        }
        "not acknowledging deliveries - still resending" => {
            "bestätigt keine Zustellungen - sende weiter".into()
        }
        "no relay accepted the frame" => "kein Relay nahm den Frame an".into(),
        "no relay accepted the subscription" => "kein Relay nahm die Subscription an".into(),
        "no 445 subscription" => "keine 445-Subscription".into(),
        other => other.to_string(),
    }
}

/// The S3 verdicts (E6): "" / "testing" / "listing" / "ok" are machine
/// states the .slint switches on — they pass through untouched; only the
/// "error: {shell}: {tail}" form localizes. Tails ride verbatim.
pub(crate) fn localize_s3_verdict(lang: i32, v: &str) -> String {
    if lang != 1 {
        return v.to_string();
    }
    match v.strip_prefix("error: ") {
        Some(e) => format!("Fehler: {}", s3_error_de(e)),
        None => v.to_string(),
    }
}

/// One `S3Error` rendering → German: the five shells by prefix, per shell
/// the known payload phrases; everything unrecognized rides verbatim.
fn s3_error_de(e: &str) -> String {
    if let Some(t) = e.strip_prefix("endpoint: ") {
        return format!("Endpunkt: {}", s3_endpoint_de(t));
    }
    if let Some(t) = e.strip_prefix("connect: ") {
        return format!("Verbindung: {t}");
    }
    if let Some(t) = e.strip_prefix("tls: ") {
        return format!("TLS: {t}");
    }
    if let Some(t) = e.strip_prefix("protocol: ") {
        return format!("Protokoll: {t}");
    }
    if let Some((status, hint)) = e.strip_prefix("http ").and_then(|r| r.split_once(": ")) {
        return format!("HTTP {status}: {}", s3_hint_de(hint));
    }
    e.to_string()
}

fn s3_endpoint_de(t: &str) -> String {
    if let Some(p) = t.strip_prefix("bad port ") {
        return format!("ungültiger Port {p}");
    }
    if let Some(h) = t.strip_prefix("bad host ") {
        return format!("ungültiger Host {h}");
    }
    match t {
        "no endpoint configured" => "kein Endpunkt konfiguriert".into(),
        "no access key configured" => "kein Access-Key konfiguriert".into(),
        "no secret key configured" => "kein Secret-Key konfiguriert".into(),
        "no bucket configured" => "kein Bucket konfiguriert".into(),
        "no host in endpoint" => "kein Host im Endpunkt".into(),
        "unterminated [IPv6] literal" => "unabgeschlossenes [IPv6]-Literal".into(),
        other => other.to_string(),
    }
}

/// An HTTP-status hint → German. The engine may append " ({S3 code})" —
/// matched by prefix so the code rides along verbatim.
fn s3_hint_de(hint: &str) -> String {
    let map: [(&str, &str); 6] = [
        (
            "the local clock is too far from the server's - fix the system time",
            "die lokale Uhr weicht zu weit von der des Servers ab - Systemzeit korrigieren",
        ),
        (
            "bucket lives at another endpoint/region (redirect)",
            "der Bucket liegt an einem anderen Endpunkt/einer anderen Region (Redirect)",
        ),
        (
            "bad request - often a region mismatch for this endpoint",
            "Bad Request - oft ein Region-Mismatch für diesen Endpunkt",
        ),
        (
            "access denied - check access key and secret",
            "Zugriff verweigert - Access-Key und Secret prüfen",
        ),
        ("unexpected status", "unerwarteter Status"),
        ("bucket `", "Bucket `"),
    ];
    for (en, de) in map {
        if let Some(rest) = hint.strip_prefix(en) {
            let rest = if en == "bucket `" {
                match rest.strip_suffix("` not found") {
                    Some(b) => return format!("Bucket `{b}` nicht gefunden"),
                    None => rest,
                }
            } else {
                rest
            };
            return format!("{de}{rest}");
        }
    }
    hint.to_string()
}

/// The Tor probe's detail sentence (E6): the engine-/probe-authored
/// verdict phrases localize by prefix, the four TargetGap clauses get
/// their own arms, and rung tails (socket errors, hosts) ride verbatim.
pub(crate) fn localize_tor_detail(lang: i32, d: &str) -> String {
    if lang != 1 || d.is_empty() {
        return d.to_string();
    }
    if let Some(g) = d.strip_prefix("no circuit was proven - ") {
        return format!("kein Circuit bewiesen - {}", tor_gap_de(g));
    }
    if let Some(g) = d.strip_prefix("nothing about Tor could be established - ") {
        return format!("nichts über Tor feststellbar - {}", tor_gap_de(g));
    }
    if let Some(net) = d
        .strip_prefix("the configured anonymity network is ")
        .and_then(|r| r.strip_suffix(", not tor - nothing was sent"))
    {
        return format!("das konfigurierte Anonymitätsnetz ist {net}, nicht tor - nichts wurde gesendet");
    }
    if d == "the resolved transport does not route over Tor" {
        return "der aufgelöste Transport routet nicht über Tor".into();
    }
    if d == "Tor is not enabled - nothing was sent" {
        return "Tor ist nicht aktiv - nichts wurde gesendet".into();
    }
    if d.starts_with("nothing was routed through the proxy") {
        return "nichts lief durch den Proxy - kein Relay aus dem Pool war über Tor \
                erreichbar"
            .into();
    }
    if d.starts_with("no SOCKS proxy to probe") {
        return "kein SOCKS-Proxy zu prüfen und kein Relay, das dieser Knoten über \
                Tor wählen darf"
            .into();
    }
    d.to_string()
}

/// The four `TargetGap` clauses → German (pinned distinct, like the
/// English originals).
pub(crate) fn tor_gap_de(g: &str) -> String {
    match g {
        "no relay is configured" => "kein Relay konfiguriert".into(),
        "no relay is confirmed yet" => "noch kein Relay bestätigt".into(),
        "the confirmed relays need non-onion dialing, which is switched off" => {
            "die bestätigten Relays brauchen Nicht-Onion-Dialing, und das ist abgeschaltet".into()
        }
        "only local relays are configured, and those bypass Tor" => {
            "nur lokale Relays konfiguriert, und die umgehen Tor".into()
        }
        other => other.to_string(),
    }
}

/// The rejoiner's recovery status line (E6): three known note phrases and
/// the failure prefixes localize; free-text error tails ride verbatim.
pub(crate) fn localize_recover_note(lang: i32, n: &str) -> String {
    if lang != 1 {
        return n.to_string();
    }
    if let Some(m) = n
        .strip_prefix("waiting for the coordinator's Welcome (")
        .and_then(|r| r.strip_suffix(" min)"))
    {
        return format!("warte auf das Welcome des Koordinators ({m} min)");
    }
    match n {
        "request sent - waiting for the coordinator's Welcome" => {
            "Anfrage gesendet - warte auf das Welcome des Koordinators".into()
        }
        "welcomed back - fetching the chain anchor" => {
            "willkommen zurück - hole den Chain-Anker".into()
        }
        other => other.to_string(),
    }
}

/// The rejoiner's recovery FAILURE line (E6): the wrapper prefixes and
/// the timeout sentence localize, the wrapped error rides verbatim.
pub(crate) fn localize_recover_failed(lang: i32, e: &str) -> String {
    if lang != 1 {
        return e.to_string();
    }
    if let Some(t) = e.strip_prefix("recovery request: ") {
        return format!("Recovery-Anfrage: {t}");
    }
    if let Some(t) = e.strip_prefix("mls welcome: ") {
        return format!("MLS-Welcome: {t}");
    }
    if e.starts_with("no Welcome arrived within 15 minutes") {
        return "kein Welcome binnen 15 Minuten - der Koordinator muss laufen und \
                die Rückkehr bestätigen"
            .into();
    }
    e.to_string()
}

/// Match one engine log-shape (`molt_engine::LogShape` semantics: constant
/// parts in order, slots between them) against a line; returns the slot
/// values on a hit. The first part anchors the start, the last the end,
/// middles bind leftmost-in-order.
fn match_shape<'a>(parts: &[&str], line: &'a str) -> Option<Vec<&'a str>> {
    let rest = line.strip_prefix(parts[0])?;
    if parts.len() == 1 {
        return rest.is_empty().then(Vec::new);
    }
    let last = parts[parts.len() - 1];
    if !rest.ends_with(last) {
        return None;
    }
    let mut body = &rest[..rest.len() - last.len()];
    let mut slots = Vec::with_capacity(parts.len() - 1);
    for mid in &parts[1..parts.len() - 1] {
        let at = body.find(mid)?;
        slots.push(&body[..at]);
        body = &body[at + mid.len()..];
    }
    slots.push(body);
    Some(slots)
}

/// One run-log line → German (E5): the engine's shape inventory paired
/// with German constant parts (same slot count — pinned); slots (names,
/// counts, URLs, free-text errors) carry over verbatim, the tone glyph
/// survives, and an unmatched line renders as itself.
pub(crate) fn localize_log_line(lang: i32, line: &str) -> String {
    if lang != 1 {
        return line.to_string();
    }
    for (en, de) in LOG_SHAPES_DE {
        if let Some(slots) = match_shape(en, line) {
            let mut out = String::new();
            for (i, part) in de.iter().enumerate() {
                out.push_str(part);
                if let Some(s) = slots.get(i) {
                    out.push_str(s);
                }
            }
            return out;
        }
    }
    line.to_string()
}

/// The German renderings, one pair per engine shape (set-equality with
/// `molt_engine::known_log_shapes()` is pinned by test). German arms
/// follow the compact-text rule and write "-" for the em dash.
pub(crate) static LOG_SHAPES_DE: &[(&[&str], &[&str])] = &[
    (
        &["→ restore started · way ", " · ", ""],
        &["→ Restore gestartet · Weg ", " · ", ""],
    ),
    (
        &["✓ chain verified · height ", " · ", "-of-", ""],
        &["✓ Chain verifiziert · Höhe ", " · ", "-von-", ""],
    ),
    (
        &["✓ backup from unix ", " (", " day(s) old) · workspace “", "” materialized"],
        &["✓ Backup von Unix ", " (", " Tag(e) alt) · Workspace “", "” materialisiert"],
    ),
    (
        &["→ the seed does not anchor this seat in the verified roster - knowledge-only restore"],
        &["→ der Seed verankert diesen Sitz nicht im verifizierten Roster - nur Wissen wird wiederhergestellt"],
    ),
    (
        &["→ knowledge restored - the workspace opens detached and reattaches automatically"],
        &["→ Wissen wiederhergestellt - der Workspace öffnet abgekoppelt und verbindet sich automatisch wieder"],
    ),
    (&["✗ restore failed: ", ""], &["✗ Restore fehlgeschlagen: ", ""]),
    (&["→ fs: read ", ""], &["→ fs: lese ", ""]),
    (&["→ s3: list ", ""], &["→ s3: liste ", ""]),
    (&["→ s3: GET ", ""], &["→ s3: hole ", ""]),
    (&["↓ ", " of ", " bytes"], &["↓ ", " von ", " Bytes"]),
    (
        &["→ decrypting + validating the blob"],
        &["→ entschlüssle + validiere den Blob"],
    ),
    (
        &["→ staged · ", " chain block(s) await verification"],
        &["→ bereitgestellt · ", " Chain-Block/Blöcke warten auf Verifikation"],
    ),
    (
        &["→ ritual opened · ", " (founder) · ", "-of-", " · ", " invite(s) minted"],
        &["→ Ritual eröffnet · ", " (Gründer) · ", "-von-", " · ", " Einladung(en) erzeugt"],
    ),
    (
        &["→ SIMULATION - no real network in this build: this node signs for every member"],
        &["→ SIMULATION - kein echtes Netz in diesem Build: dieser Knoten signiert für jedes Mitglied"],
    ),
    (
        &["→ share each link off-band over a private channel - the ritual waits for the activations"],
        &["→ jeden Link off-band über einen privaten Kanal teilen - das Ritual wartet auf die Aktivierungen"],
    ),
    (
        &["✓ roster sealed by everyone · workspace created"],
        &["✓ Roster von allen versiegelt · Workspace erstellt"],
    ),
    (&["✗ founding failed: ", ""], &["✗ Gründung fehlgeschlagen: ", ""]),
    (&["✓ recovery phrase backed up"], &["✓ Recovery-Phrase gesichert"]),
    (
        &["⚠ the relay pool changed - minted invites still name the old relays; cancel and re-mint"],
        &["⚠ der Relay-Pool hat sich geändert - erzeugte Einladungen nennen noch die alten Relays; abbrechen und neu erzeugen"],
    ),
    (
        &["→ this node has ", " dialable relays; the invite and the Welcome carry the first ", " (pool order = priority - reorder in Settings)"],
        &["→ dieser Knoten hat ", " wählbare Relays; Einladung und Welcome tragen die ersten ", " (Pool-Reihenfolge = Priorität - unter Einstellungen umsortieren)"],
    ),
    (
        &["✗ ", " does not reach ", " of ", " pool relays - ", ""],
        &["✗ ", " erreicht ", " von ", " Pool-Relays nicht - ", ""],
    ),
    (
        &["⚠ ", " landed on ", " of ", " relays - ", ""],
        &["⚠ ", " landete auf ", " von ", " Relays - ", ""],
    ),
    (
        &["✓ direct mesh established · ", " peer(s)"],
        &["✓ direktes Mesh steht · ", " Peer(s)"],
    ),
    (
        &["→ the group is born · welcomes sent to every member"],
        &["→ die Gruppe ist geboren · Welcomes an alle Mitglieder gesendet"],
    ),
    (
        &["✗ invite ", ": a second activation by ", " did not verify - ignored"],
        &["✗ Einladung ", ": eine zweite Aktivierung durch ", " verifizierte nicht - ignoriert"],
    ),
    (
        &["✗ invite ", ": the group already formed around the first activation - cancel and re-mint to let ", " back in"],
        &["✗ Einladung ", ": die Gruppe hat sich um die erste Aktivierung gebildet - abbrechen und neu erzeugen, um ", " wieder hereinzulassen"],
    ),
    (
        &["✗ invite ", " was activated a second time (by ", ") - that link is spent, they need an unused one"],
        &["✗ Einladung ", " wurde ein zweites Mal aktiviert (durch ", ") - dieser Link ist verbraucht, ein unbenutzter ist nötig"],
    ),
    (
        &["· invite ", " activated by ", " - checking"],
        &["· Einladung ", " aktiviert durch ", " - prüfe"],
    ),
    (
        &["✗ invite ", ": the request claims a transport key it did not sign with - refused"],
        &["✗ Einladung ", ": die Anfrage nennt einen Transport-Schlüssel, mit dem sie nicht signiert hat - abgelehnt"],
    ),
    (
        &["✗ invite ", ": the ticket code does not match - refused (wrong, edited or foreign link)"],
        &["✗ Einladung ", ": der Ticket-Code passt nicht - abgelehnt (falscher, veränderter oder fremder Link)"],
    ),
    (
        &["✗ invite ", ": malformed transport key (", ") - refused, the ticket stays usable"],
        &["✗ Einladung ", ": fehlgeformter Transport-Schlüssel (", ") - abgelehnt, das Ticket bleibt nutzbar"],
    ),
    (
        &["✗ invite ", ": the name ", " is already taken in this founding - refused"],
        &["✗ Einladung ", ": der Name ", " ist in dieser Gründung schon vergeben - abgelehnt"],
    ),
    (
        &["✗ invite ", ": that transport key is already used by another seat - refused"],
        &["✗ Einladung ", ": dieser Transport-Schlüssel wird schon von einem anderen Sitz benutzt - abgelehnt"],
    ),
    (
        &["✗ invite ", ": no usable reply address in the request - refused"],
        &["✗ Einladung ", ": keine nutzbare Antwort-Adresse in der Anfrage - abgelehnt"],
    ),
    (
        &["✗ invite ", ": the key package does not match the identity in the request - refused"],
        &["✗ Einladung ", ": das Key-Package passt nicht zur Identität in der Anfrage - abgelehnt"],
    ),
    (
        &["· invite ", " re-activated by ", " - the earlier attempt is replaced"],
        &["· Einladung ", " erneut aktiviert durch ", " - der frühere Versuch ist ersetzt"],
    ),
    (
        &["→ ", " activated invite ", " · key received"],
        &["→ ", " aktivierte Einladung ", " · Schlüssel empfangen"],
    ),
    (
        &["→ every member has joined · propose the charter to seal"],
        &["→ alle Mitglieder sind beigetreten · zum Versiegeln die Charter vorschlagen"],
    ),
    (
        &["→ charter proposed · awaiting every member's ratification"],
        &["→ Charter vorgeschlagen · warte auf die Ratifikation aller Mitglieder"],
    ),
    (
        &["✗ a decline for invite ", " came from ", ", who does not hold that seat - ignored"],
        &["✗ eine Ablehnung für Einladung ", " kam von ", ", das diesen Sitz nicht hält - ignoriert"],
    ),
    (
        &["✗ ", " declined the charter · cancel and re-mint to change it"],
        &["✗ ", " hat die Charter abgelehnt · zum Ändern abbrechen und neu erzeugen"],
    ),
    (
        &["✗ the ritual is over - this republic must be founded anew (close and re-mint)"],
        &["✗ das Ritual ist vorbei - diese Republik muss neu gegründet werden (schließen und neu erzeugen)"],
    ),
    (
        &["→ charter proposed · sealing the roster for ratification"],
        &["→ Charter vorgeschlagen · versiegle den Roster zur Ratifikation"],
    ),
    (
        &["✓ ", " signed the roster · seat sealed"],
        &["✓ ", " hat den Roster signiert · Sitz versiegelt"],
    ),
    (
        &["✓ ", " secured their key"],
        &["✓ ", " hat den Schlüssel gesichert"],
    ),
    (
        &["✓ the group channel is back"],
        &["✓ der Gruppenkanal ist zurück"],
    ),
    (
        &["⚠ cannot hear the group channel - ", " · still retrying"],
        &["⚠ der Gruppenkanal ist nicht hörbar - ", " · versuche weiter"],
    ),
    (
        &["⧗ waiting for the genesis · ", ""],
        &["⧗ warte auf die Genesis · ", ""],
    ),
    (
        &["✓ recovery phrase backed up · waiting for the others"],
        &["✓ Recovery-Phrase gesichert · warte auf die anderen"],
    ),
    (
        &["✓ sealed - back up your recovery phrase to enter"],
        &["✓ versiegelt - zum Eintreten die Recovery-Phrase sichern"],
    ),
    (&["✗ join failed: ", ""], &["✗ Beitritt fehlgeschlagen: ", ""]),
    (
        &["✓ the founder accepted your join · waiting for the deliberation"],
        &["✓ der Gründer hat den Beitritt angenommen · warte auf die Beratung"],
    ),
    (
        &["→ charter proposed: “", "” · review and confirm to join"],
        &["→ Charter vorgeschlagen: “", "” · prüfen und bestätigen zum Beitritt"],
    ),
    (
        &["✓ you ratified the charter · sealing your signature"],
        &["✓ Charter ratifiziert · versiegle die Signatur"],
    ),
    (
        &["→ save your recovery phrase - re-type it to confirm"],
        &["→ Recovery-Phrase sichern - zur Bestätigung erneut eintippen"],
    ),
    (&["✗ you declined the charter"], &["✗ Charter abgelehnt"]),
    (
        &["✗ the ritual is over - this republic must be founded anew"],
        &["✗ das Ritual ist vorbei - diese Republik muss neu gegründet werden"],
    ),
    (&["→ dialable here: ", ""], &["→ hier wählbar: ", ""]),
    (&["→ ", "  dialable"], &["→ ", "  wählbar"]),
    (&["→ ", "  not in relay pool"], &["→ ", "  nicht im Relay-Pool"]),
    (&["→ ", "  not confirmed"], &["→ ", "  nicht bestätigt"]),
    (
        &["→ ", "  clearnet/local dialing off"],
        &["→ ", "  Clearnet/Lokal-Dialing aus"],
    ),
];

/// Declare the whole localized string table in ONE place: the macro
/// generates the `Lexicon` struct, its English and German tables, and
/// `apply_strings` (which pushes every entry into the Slint `Strings`
/// global). Adding a string = one line here + its declaration in
/// `theme.slint` — the four-places-per-string era is over.
macro_rules! lexicon {
    ($( $field:ident: $en:expr, $de:expr; )+) => {
        /// The full localized string table for one language.
        pub(crate) struct Lexicon {
            $( pub(crate) $field: &'static str, )+
        }

        impl Lexicon {
            pub(crate) fn en() -> Self {
                Lexicon { $( $field: $en, )+ }
            }

            pub(crate) fn de() -> Self {
                Lexicon { $( $field: $de, )+ }
            }
        }

        /// Push the localized string table for `lang` (0 = English,
        /// 1 = German) into the Slint `Strings` global.
        pub(crate) fn apply_strings(ui: &AppWindow, lang: i32) {
            let l = if lang == 1 { Lexicon::de() } else { Lexicon::en() };
            let s = ui.global::<Strings>();
            paste::paste! {
                $( s.[<set_ $field>](l.$field.into()); )+
            }
        }
    };
}

lexicon! {
    choice_title: "Welcome", "Willkommen";
    // card titles split as hotkey letter + rest: the letter renders
    // underlined and typing it activates the card
    choice_create_key: "C", "G";
    choice_create_rest: "reate", "ründen";
    choice_create_sub: "A new workspace", "Workspace erstellen";
    choice_open_key: "O", "Ö";
    choice_open_rest: "pen", "ffnen";
    choice_open_sub: "Open a local workspace", "Lokalen Workspace öffnen";
    choice_backup_lead: "Restore from ", "";
    choice_backup_key: "b", "B";
    choice_backup_rest: "ackup", "ackup wiederherstellen";
    choice_restore_lead: "(", "(";
    choice_restore_key: "R", "R";
    choice_restore_rest: "e)Join / Recovery", "e)Join / Recovery";
    choice_restore_sub: "Join with an invite, or come back to a seat you hold", "Mit einer Einladung beitreten, oder auf deinen Sitz zurück";
    nav_back: "Back", "Zurück";
    wiz_step_setup: "Setup", "Einrichtung";
    wiz_step_invites: "Invites", "Einladungen";
    wiz_step_charter: "Charter & Features", "Charta & Features";
    wiz_step_enter: "Enter", "Eintreten";
    wiz_sealed_note: "Sealed by all members", "Von allen Mitgliedern besiegelt";
    wiz_step_invite: "Invite", "Einladung";
    wiz_step_joining: "Joining", "Beitritt";
    wiz_step_way: "Way in", "Zugang";
    wiz_step_phrase: "Phrase", "Phrase";
    wiz_step_restore: "Restore", "Wiederherstellung";
    field_network: "Anonymity network", "Anonymitäts-Netzwerk";
    not_implemented_yet: "not yet", "noch nicht";
    field_tor_mode: "Tor mode", "Tor-Modus";
    field_tor_port: "Tor SOCKS port", "Tor-SOCKS-Port";
    smp_testing: "testing…", "teste…";
    field_threshold: "Threshold", "Schwelle";
    field_members: "Members", "Mitglieder";
    field_language: "Language", "Sprache";
    field_theme: "Theme", "Erscheinungsbild";
    field_font_app: "App font size", "Schriftgröße App";
    field_font_nav: "Navigator font size", "Schriftgröße Navigator";
    field_font_editor: "Editor font size", "Schriftgröße Editor";
    field_workspace_dir: "Workspace directory", "Workspace-Verzeichnis";
    field_mcp_port: "MCP port", "MCP-Port";
    field_mcp_allow: "Allowed client IPs", "Erlaubte Client-IPs";
    field_mcp_token: "API token", "API-Token";
    set_rotate: "Rotate", "Rotieren";
    set_token_note: "Sent by clients as the token in initialize. Rotate writes a fresh token to config.toml; it takes effect on restart.", "Von Clients als token im initialize gesendet. Rotieren schreibt ein frisches Token in die config.toml; es gilt ab dem Neustart.";
    peek_show: "Reveal", "Anzeigen";
    peek_hide: "Hide", "Verbergen";
    field_headless: "Headless (MCP only, no GUI)", "Headless (nur MCP, keine GUI)";
    cw_title: "Found a new Republic", "Neue Republik gründen";
    cw_grp_republic: "Workspace", "Workspace";
    ph_ws_name: "My new republic", "Meine neue Republik";
    ph_member: "my name", "mein Name";
    ph_seed: "word1 word2 word3 …", "wort1 wort2 wort3 …";
    field_member_name: "My user name", "Mein Benutzername";
    cw_grp_rule: "Approval Rules", "Zustimmungsregeln";
    cw_rule_hint: "Gated changes apply only once enough members approve.", "Geschützte Änderungen gelten erst, wenn genug Mitglieder zustimmen.";
    cw_rule_warn: "not recommended", "nicht empfohlen";
    cw_rule_a: "Every gated change needs", "Jede geschützte Änderung braucht";
    cw_rule_b: "of", "von";
    cw_rule_c: "approvals.", "Stimmen.";
    // Relays do not federate: two members hear each other only if they both
    // dial a relay in common. Stated at CREATE time because that is the last
    // moment the choice is cheap (§10.15, user-ratified 2026-08-02).
    cw_grp_relays: "Relays", "Relays";
    cw_relays_hint: "The group needs ONE relay every member can reach. A self-hosted relay must be in every member's pool before they join.", "Die Gruppe braucht EIN Relay, das jedes Mitglied erreicht. Ein eigenes Relay muss vor dem Beitritt im Pool jedes Mitglieds stehen.";
    cw_relays_none: "No relay this node can dial - add one in Settings.", "Kein erreichbarer Relay - in den Einstellungen einen hinzufügen.";
    cw_relays_toggle: "Use for this republic", "Für diese Republik verwenden";
    cw_grp_transport: "Anonymization Layer", "Anonymisierungsschicht";
    cw_transport_hint: "How this node reaches the other members - one global setting for every republic.", "Wie dieser Node die anderen Mitglieder erreicht - eine globale Einstellung für jede Republik.";
    // this panel is about the ANONYMITY layer only (tor/none) — never the
    // relay pool, which is its own settings tab. Both the label and the
    // deep-link hint name that tab exactly, so "Netzwerk" can no longer be
    // read as "Nostr-Relays".
    cw_net_label: "Anonymity network", "Anonymitäts-Netzwerk";
    cw_net_ok_tor: "Anonymized via Tor circuits.", "Anonymisiert via Tor-Circuits.";
    cw_net_warn: "Not anonymized - peers see your IP.", "Nicht anonymisiert - Peers sehen deine IP.";
    cw_net_hint_settings: "Global setting - change it under Settings → Anonymity network.", "Globale Einstellung - ändern unter Einstellungen → Anonymitäts-Netzwerk.";
    cw_found: "Begin ritual", "Ritual beginnen";
    cw_invites: "Invites", "Einladungen";
    cw_invites_hint: "One link per future member - share each once, over a private channel.", "Ein Link pro künftigem Mitglied - jeden nur einmal teilen, über einen privaten Kanal.";
    cw_members_title: "Members", "Mitglieder";
    cw_sealed_word: "sealed", "versiegelt";
    cw_sim_badge: "SIMULATION", "SIMULATION";
    cw_ritual_hint: "Share each link once, over a private channel. The republic is created once every member has activated their link and signed the roster.", "Teile jeden Link einmal, über einen privaten Kanal. Die Republik entsteht, sobald jedes Mitglied seinen Link aktiviert und die Mitgliederliste signiert hat.";
    cw_provisioning: "Preparing the invite link…", "Invite-Link wird vorbereitet…";
    cw_failed_title: "The founding cannot continue", "Die Gründung kann nicht fortgesetzt werden";
    cw_failed_hint: "Close and found anew once it is resolved.", "Schließen und neu gründen, sobald es behoben ist.";
    // the button jumps to the anonymity tab (set-tab = 4) — it must not
    // promise the relay settings that now live one tab further
    cw_open_net_settings: "Open anonymity settings", "Anonymitäts-Einstellungen öffnen";
    cw_open_relay_settings: "Relay settings", "Relay-Einstellungen";
    cw_ritual_hint_sim: "No real network yet: this node simulates the other members - it auto-activates and signs for them. Nothing is shared with anyone. Real members arrive with the Nostr transport (N4).", "Noch kein echtes Netzwerk: dieser Knoten simuliert die anderen Mitglieder - er aktiviert und signiert selbst für sie. Es wird nichts mit jemandem geteilt. Echte Mitglieder kommen mit dem Nostr-Transport (N4).";
    cw_log_title: "Ritual log", "Ritual-Protokoll";
    cw_charter_title: "Agree on the charter", "Auf die Satzung einigen";
    cw_charter_step: "Agree on the charter", "Einigt euch auf die Satzung";
    cw_seed_confirm_title: "Save your recovery phrase", "Sichere deine Wiederherstellungs-Phrase";
    cw_backup_confirm: "Confirm backup", "Backup bestätigen";
    cw_backup_wait: "Backup confirmed - waiting for every member's confirmation", "Backup bestätigt - warte auf die Bestätigung aller Mitglieder";
    cw_seed_confirm_hint: "It is the only way back to this seat. Re-type it to continue.", "Sie ist der einzige Weg zurück zu deinem Sitz. Gib sie zur Bestätigung erneut ein.";
    cw_seed_confirm_ph: "Re-type the phrase", "Phrase erneut eingeben";
    cw_ratify_wait: "The charter is with the members - waiting for their signatures…", "Die Satzung liegt bei den Mitgliedern - warte auf ihre Unterschriften…";
    cw_charter_name_label: "Republic name", "Name der Republik";
    cw_charter_name_ph: "Final republic name", "Endgültiger Name der Republik";
    cw_charter_agenda_ph: "Agenda / charter - what this republic is for", "Agenda / Satzung - wofür diese Republik steht";
    cw_charter_hint: "Write the republic's agenda - every member must sign it.", "Schreib die Agenda der Republik - alle Mitglieder müssen sie unterzeichnen.";
    cw_abort_title: "Abort the founding ritual?", "Gründungsritual abbrechen?";
    cw_abort_body: "Every distributed invite link becomes invalid and the ritual ends for all participants. You can start a fresh founding afterwards.", "Alle verteilten Einladungslinks werden ungültig und das Ritual endet für alle Beteiligten. Danach kann eine neue Gründung gestartet werden.";
    cw_abort_confirm: "Abort ritual", "Ritual abbrechen";
    cw_declined_title: "The founding is over", "Die Gründung ist beendet";
    cw_declined_hint: "A member declined the charter - close and found anew.", "Ein Mitglied hat die Satzung abgelehnt - schließen und neu gründen.";
    cw_propose: "Propose & seal", "Vorschlagen & versiegeln";
    cw_features: "Features", "Features";
    feat_chat: "Chat", "Chat";
    feat_memory: "Shared Memory", "Shared Memory";
    feat_quests: "Kanban", "Kanban";
    feat_vault: "Vault", "Vault";
    feat_wallet: "Wallet", "Wallet";
    // suffix on an enable-able feature whose pane is still a mock (vault)
    feat_mock: " (ui mock)", " (ui mock)";
    jw_back_to_start: "Back to start", "Zurück zum Start";
    jw_ratify_title: "Ratify the charter", "Satzung ratifizieren";
    jw_ratify_confirm: "Confirm & join", "Bestätigen & beitreten";
    jw_ratify_decline: "Decline", "Ablehnen";
    jw_ratify_agenda_empty: "(no agenda set)", "(keine Agenda festgelegt)";
    const_immutable: "Immutable · ratified by everyone at founding", "Unveränderlich · von allen bei der Gründung ratifiziert";
    const_charter_title: "Charter", "Satzung";
    const_no_agenda: "(founded without a written charter)", "(ohne schriftliche Satzung gegründet)";
    const_signatories: "Founding members · ratified by all", "Gründungsmitglieder · von allen ratifiziert";
    enter_republic: "Enter republic", "Republik betreten";
    org_reachable: "reachable", "erreichbar";
    org_approvals: "Approvals", "Approvals";
    oa_col_surface: "Surface", "Bereich";
    oa_pending: "Pending", "Offen";
    oa_denied: "Denied", "Abgelehnt";
    oa_list_pending: "List pending", "Offene zeigen";
    org_edit: "Edit", "Bearbeiten";
    ol_title: "Republic image", "Bild der Republik";
    ol_body: "Pick a new image via the file dialog, or remove the current one. Either way the change is a gated proposal the members approve by threshold. The image itself (about 64 KiB) travels inside the proposal, so every member sees exactly what they approve - once applied, it shows on every device.", "Wähle über den Datei-Dialog ein neues Bild oder entferne das aktuelle. Beides ist eine geschützte Änderung, der die Mitglieder per Schwelle zustimmen. Das Bild selbst (rund 64 KiB) reist im Vorschlag mit - jedes Mitglied sieht genau, worüber es abstimmt; nach dem Anwenden erscheint es auf jedem Gerät.";
    ol_remove: "Remove image", "Bild entfernen";
    ol_current: "Current image", "Aktuelles Bild";
    ol_none: "No image set.", "Kein Bild gesetzt.";
    ol_pick: "Choose…", "Auswählen…";
    oc_title: "Edit charter", "Satzung bearbeiten";
    oc_body: "The charter was ratified by everyone at the founding - an edit is a gated change: the draft becomes a proposal the members approve by threshold. Once applied, every view shows the new charter; the founding charter stays immutable in block 0.", "Die Satzung wurde bei der Gründung von allen ratifiziert - eine Bearbeitung ist eine geschützte Änderung: der Entwurf wird ein Vorschlag, dem die Mitglieder per Schwelle zustimmen. Nach dem Anwenden zeigt jede Ansicht die neue Satzung; die Gründungssatzung bleibt unveränderlich in Block 0.";
    oc_propose: "Propose change", "Änderung vorschlagen";
    toast_proposed: "Proposed - awaiting approvals", "Vorgeschlagen - wartet auf Zustimmungen";
    om_col_id: "ID", "ID";
    om_col_pk: "Public key", "Public Key";
    om_col_last: "Last seen", "Zuletzt gesehen";
    om_col_uploads: "Uploads", "Uploads";
    om_me: "(that's me)", "(das bin ich)";
    om_col_recovery: "Recovery link", "Recovery-Link";
    mp_col_desc: "Description", "Beschreibung";
    mp_img_edit: "Edit picture", "Bild bearbeiten";
    mp_desc_edit: "Edit description", "Beschreibung bearbeiten";
    mp_img_title: "Your picture", "Dein Bild";
    mp_img_body: "The members approve it.", "Die Mitglieder stimmen zu.";
    mp_desc_title: "Your description", "Deine Beschreibung";
    mp_desc_body: "The members approve it.", "Die Mitglieder stimmen zu.";
    mp_desc_ph: "One line about you", "Eine Zeile über dich";
    mp_img_too_big: "Picture too large for this republic", "Bild zu groß für diese Republik";
    ou_col_user: "User", "Nutzer";
    ou_col_date: "Date", "Datum";
    ou_col_file: "Filename", "Dateiname";
    ou_col_type: "Type", "Typ";
    ou_col_size: "Size", "Größe";
    ou_col_checksum: "Checksum", "Checksum";
    ou_col_download: "Download", "Download";
    ou_col_expires: "Expires in", "Läuft ab in";
    ou_gone: "gone", "weg";
    ou_download: "Download", "Download";
    ou_offline: "user offline", "Nutzer offline";
    ou_empty: "No files shared yet.", "Noch keine Dateien geteilt.";
    ou_filter_ph: "Filter: user, filename or checksum", "Filter: Nutzer, Dateiname oder Checksum";
    // the paged lists' "Page x of y" label, split around the two numbers
    pg_page: "Page", "Seite";
    pg_of: "of", "von";
    ou_no_match: "No uploads match the filter.", "Keine Uploads passen zum Filter.";
    orn_title: "Rename republic", "Republik umbenennen";
    orn_body: "The name was ratified at the founding - renaming is a gated change: the draft becomes a proposal the members approve by threshold. Once applied, the republic shows its new name everywhere; its identity (the republic id) never changes.", "Der Name wurde bei der Gründung ratifiziert - eine Umbenennung ist eine geschützte Änderung: der Entwurf wird ein Vorschlag, dem die Mitglieder per Schwelle zustimmen. Nach dem Anwenden trägt die Republik überall den neuen Namen; ihre Identität (die Republik-ID) ändert sich nie.";
    pc_current: "Current", "Ist-Stand";
    pc_proposed: "Proposed", "Soll-Stand";
    pc_discuss: "Discussion", "Diskussion";
    ch_readonly: "read-only - the vote is decided", "nur lesen - die Abstimmung ist entschieden";
    pc_proposal: "Proposal:", "Vorschlag:";
    pc_img_hint: "Click to view the proposed image", "Klicken zum Anzeigen des vorgeschlagenen Bilds";
    pc_img_missing: "The proposed image could not be decoded.", "Das vorgeschlagene Bild konnte nicht dekodiert werden.";
    pc_img_save: "Save image to disk", "Bild auf der Platte speichern";
    os_founded: "Founded", "Gegründet";
    os_consensus: "Consensus", "Konsens";
    cv_shrink: "Shrink", "Verkleinern";
    ocs_title: "Settings", "Einstellungen";
    ocs_chat_retention: "Delete chat after", "Chat löschen nach";
    ocs_relays: "Relays", "Relays";
    ocs_days: "days", "Tage";
    ocr2_title: "Change relay pool", "Relay-Pool ändern";
    ocr2_body: "Every member must reach at least one of these. The change becomes a threshold vote.", "Jedes Mitglied muss mindestens eines davon erreichen. Die Änderung wird eine Schwellen-Abstimmung.";
    ocr2_add: "Add relay", "Relay hinzufügen";
    ocr2_remove: "Remove", "Entfernen";
    om_coarse: "Presence over relays is coarse: last seen at the last message, not pinged live.", "Präsenz über Relays ist grob: zuletzt gesehen bei der letzten Nachricht, kein Live-Ping.";
    cs_files_off: "File sharing is not available over relays yet", "Dateifreigabe über Relays gibt es noch nicht";
    ocr_title: "Change chat deletion period", "Chat-Löschfrist ändern";
    ocr_body: "Chat is ephemeral: messages older than this are deleted on every member.", "Chat ist flüchtig: ältere Nachrichten werden bei allen Mitgliedern gelöscht.";
    ou_note: "Only metadata is shared - the bytes move user-to-user over an encrypted transfer when a member downloads, as long as the sharer keeps the file. The share expires with the chat retention window.", "Geteilt werden nur Metadaten - die Bytes wandern user-to-user über eine verschlüsselte Übertragung, wenn ein Mitglied lädt, solange der Teilende die Datei behält. Der Share läuft mit dem Chat-Aufbewahrungsfenster ab.";
    ow_title: "Open local workspace", "Lokalen Workspace öffnen";
    ow_empty: "No local workspaces found.", "Keine lokalen Workspaces gefunden.";
    ow_change_folder: "Change folder", "Ordner wechseln";
    ow_col_name: "Name", "Name";
    ow_col_sync: "Last sync", "Letzter Sync";
    ow_col_backup: "Backup", "Backup";
    ow_col_status: "Status", "Status";
    ow_enc: "encrypted", "verschlüsselt";
    ow_unenc: "unencrypted", "entschlüsselt";
    ow_encrypt: "Encrypt", "Verschlüsseln";
    ow_decrypt: "Decrypt", "Entschlüsseln";
    dw_title: "Decrypt workspace", "Workspace entschlüsseln";
    dw_body: "Enter the recovery phrase to decrypt this workspace on disk: it is verified against the workspace, the keys are restored, and the workspace can be opened again. A wrong phrase changes nothing.", "Gib die Wiederherstellungs-Phrase ein, um diesen Workspace auf der Platte zu entschlüsseln: sie wird gegen den Workspace geprüft, die Schlüssel werden wiederhergestellt, und der Workspace lässt sich wieder öffnen. Eine falsche Phrase ändert nichts.";
    ew_title: "Encrypt workspace", "Workspace verschlüsseln";
    ew_body: "Enter the recovery phrase to seal this workspace on disk: it is verified first, then the device-stored keys are removed - afterwards only the phrase opens this workspace again.", "Gib die Wiederherstellungs-Phrase ein, um diesen Workspace auf der Platte zu versiegeln: sie wird zuerst geprüft, dann werden die gerätegespeicherten Schlüssel entfernt - danach öffnet nur noch die Phrase diesen Workspace.";
    ow_open: "Open", "Öffnen";
    ow_delete: "Delete", "Löschen";
    ow_select_hint: "Select a republic to see its status.", "Wähle eine Republik, um ihren Status zu sehen.";
    ow_s3_on: "S3 active", "S3 aktiv";
    ow_s3_off: "No S3", "Kein S3";
    ow_grp_backup: "Backup", "Backup";
    ow_grp_seed: "Seed", "Seed";
    ow_seed_missing: "No seed is stored on this device - only your written-down phrase can restore this workspace.", "Auf diesem Gerät ist kein Seed gespeichert - nur deine notierte Phrase kann diesen Workspace wiederherstellen.";
    ow_members: "Members", "Mitglieder";
    ow_backup_cfg: "Settings", "Einstellungen";
    ow_export: "Manual backup", "Manuelles Backup";
    ow_export_note: "Exported:", "Exportiert:";
    ow_export_running: "Exporting…", "Exportiere…";
    ow_export_failed: "Export failed:", "Export fehlgeschlagen:";
    ow_export_skipped: "not included:", "nicht enthalten:";
    ow_seed_show: "Reveal seed", "Seed zeigen";
    ow_seed_hide: "Hide seed", "Seed verbergen";
    ow_seed_note: "Every secret key of this workspace is derived deterministically from this seed. Never share it.", "Alle geheimen Schlüssel dieses Workspace werden deterministisch aus diesem Seed abgeleitet. Niemals weitergeben.";
    ow_copy: "Copy", "Kopieren";
    ow_hold_tip: "Hold to reveal", "Halten zum Anzeigen";
    toast_copied: "Copied to clipboard", "In die Zwischenablage kopiert";
    del_ws_title: "Delete workspace?", "Workspace löschen?";
    del_ws_body: "This moves the republic's folder into the trash on this device - recoverable for 30 days, then purged. Type its name to confirm.", "Dies verschiebt den Ordner der Republik in den Papierkorb dieses Geräts - 30 Tage wiederherstellbar, danach entfernt. Tippe zur Bestätigung ihren Namen aus.";
    del_ws_confirm: "Delete permanently", "Endgültig löschen";
    bk_title: "Manual backup", "Manuelles Backup";
    bk_body: "The whole workspace is written to this location as one encrypted file - history, chain, and (when stored here) the recovery seed. Live group/transport state is never included: restoring reads everything, rejoining runs the recovery ritual. Caution: this backup + its passphrase can restore your seat like the recovery phrase - guard both.", "Der gesamte Workspace wird als eine verschlüsselte Datei an diesen Ort geschrieben - Historie, Chain und (wenn hier gespeichert) der Recovery-Seed. Live-Gruppen-/Transport-Zustand ist nie enthalten: Wiederherstellen macht alles lesbar, der Wiederbeitritt läuft über das Recovery-Ritual. Achtung: dieses Backup + seine Passphrase kann deinen Sitz wiederherstellen wie die Recovery-Phrase - beides gut verwahren.";
    bk_path: "Target file", "Zieldatei";
    bk_pass: "Export passphrase (min. 10 characters)", "Export-Passphrase (mind. 10 Zeichen)";
    bk_confirm: "Save backup", "Backup speichern";
    field_s3_backup: "Automatic S3 backup", "Automatisches S3-Backup";
    field_s3_endpoint: "S3 endpoint", "S3-Endpunkt";
    jw_title: "Join by invite", "Per Einladung beitreten";
    jw_grp_invite: "Invite link", "Einladungslink";
    jw_invite_hint: "Paste the one-time molt:// invite another member created for you.", "Füge die einmalige molt://-Einladung ein, die ein Mitglied für dich erstellt hat.";
    jw_ok: "Invite looks OK.", "Einladung sieht OK aus.";
    jw_grp_preview: "You are joining …", "Du trittst bei …";
    jw_preview_hint: "Details are exchanged during the handshake.", "Details werden beim Handshake ausgetauscht.";
    jw_invited_by: "invited by", "eingeladen von";
    jw_adopt_relays: "Add the republic's relays", "Relays der Republik hinzufügen";
    // onion relays need no exposure decision, so they are confirmed outright;
    // a clearnet one still waits for the acknowledgement in Settings
    jw_adopt_done: "Added. Clearnet relays still need confirming in Settings.", "Hinzugefügt. Clearnet-Relays müssen noch in den Einstellungen bestätigt werden.";
    jw_join: "Join republic", "Republik beitreten";
    jw_busy_title: "Joining the republic", "Beitritt zur Republik";
    jw_busy_cancel: "Cancel", "Abbrechen";
    jw_ph1: "Contacting the inviter…", "Kontaktiere den Einlader…";
    jw_ph2: "Synchronizing ritual, please wait…", "Synchronisiere Ritual, bitte warten…";
    jw_ph_founder: "Waiting for the founder - the charter is being written…", "Warte auf den Gründer - die Satzung entsteht…";
    jw_ph3: "Syncing surfaces…", "Synchronisiere Surfaces…";
    // no cause here: the headline above carries it, and "invite rejected" was
    // wrong for the commonest refusal of all — the relay gate, which turns the
    // join away before anything is sent
    jw_failed: "Failed", "Fehlgeschlagen";
    om_recover_link: "Recovery link", "Recovery-Link";
    rlk_title: "Recovery link", "Recovery-Link";
    rlk_body: "Hand this link to the returning member so they can rejoin this republic from a new device.", "Gib diesen Link dem zurückkehrenden Mitglied, damit es dieser Republik von einem neuen Gerät wieder beitreten kann.";
    rlk_caution: "Caution: share this link only over a secret channel. It is single-use and becomes invalid again when this application restarts.", "Achtung, dieser Link sollte nur über einen geheimen Kanal geteilt werden. Er ist einmalig nutzbar und wird nach Neustart dieser Anwendung wieder ungültig.";
    rlk_pending: "Creating the link…", "Link wird erstellt…";
    rlk_pending_hint: "The returning member does not need to be online - a recovery link is made for someone who is unreachable.", "Das zurückkehrende Mitglied muss dafür nicht online sein - ein Recovery-Link ist ja gerade für ein unerreichbares Mitglied gedacht.";
    rlk_failed_mesh: "The link could not be created: this device is not on the republic's mesh. Reopen the republic, then try again.", "Der Link konnte nicht erstellt werden: Dieses Gerät ist nicht im Mesh der Republik. Republik neu öffnen, dann erneut versuchen.";
    rlk_failed_prefix: "The link could not be created: ", "Der Link konnte nicht erstellt werden: ";
    rv_running_note: "Waiting for the survivors - times out after ~15 min.", "Warte auf die verbliebenen Mitglieder - Timeout nach ~15 min.";
    rv_approvals: "Approvals", "Zustimmungen";
    rv_auto_note: "members approve automatically when online", "Mitglieder stimmen automatisch zu, sobald sie online sind";
    rv_failed_hint: "Recovery links are single-use - ask any surviving member for a fresh one and try again.", "Recovery-Links sind einmalig - bitte ein verbliebenes Mitglied um einen neuen und versuch es erneut.";
    set_token_failed: "Could not mint a token - the old one still applies.", "Konnte kein Token erzeugen - das alte gilt weiter.";
    rw_title: "(Re)Join / Recovery", "(Re)Join / Recovery";
    rw_title_s3: "Restore from backup", "Backup wiederherstellen";
    rw_seed: "Recovery phrase", "Wiederherstellungs-Phrase";
    rw_paste: "Paste", "Einfügen";
    rw_seed_hint: "The phrase of the seat you are coming back to.", "Die Phrase des Sitzes, auf den du zurückkommst.";
    rw_seed_hint_s3: "The phrase the backup was sealed with.", "Die Phrase, mit der das Backup versiegelt wurde.";
    rw_back: "Back", "Zurück";
    rw_continue: "Continue", "Weiter";
    rw_link_title: "The invite or recovery link you were given", "Der Einladungs- oder Recovery-Link, den du bekommen hast";
    rw_link_ph: "Paste your molt:// link here", "molt://-Link hier einfügen";
    rw_join_running: "A join is already running.", "Ein Beitritt läuft bereits.";
    rw_founding_running: "A founding is already running.", "Eine Gründung läuft bereits.";
    rw_recovery_running: "A recovery is already running.", "Eine Wiederherstellung läuft bereits.";
    rw_join_awaits: "A join is waiting for your confirmation.", "Ein Beitritt wartet auf deine Bestätigung.";
    rw_join_goto: "Show", "Anzeigen";
    rw_link_join: "Invite to", "Einladung zu";
    rw_link_recover: "Recovery for", "Wiederherstellung für";
    rw_link_unknown: "Not a usable molt:// link.", "Kein brauchbarer molt://-Link.";
    rw_link_missing_relays: "of the link's relays are not in your pool", "der Link-Relays fehlen in deinem Pool";
    rw_link_name_ph: "Your name…", "Dein Name…";
    rw_via_s3: "Online-restore via S3", "Online-Restore via S3";
    rw_s3_hint: "Pulls the encrypted backup from the S3 bucket in the storage settings; the chain is verified before anything materializes.", "Holt das verschlüsselte Backup aus dem S3-Bucket der Speicher-Einstellungen; die Chain wird vor dem Anlegen verifiziert.";
    rw_s3_none: "No S3 endpoint configured.", "Kein S3-Endpunkt konfiguriert.";
    rw_s3_ok: "reachable", "erreichbar";
    // honest endpoint status: "reachable" is only claimed after a REAL
    // probe (session.s3_test == "ok"); before that the state is untested
    rw_s3_untested: "not tested - use Test under Settings → S3 config", "ungetestet - Test unter Einstellungen → S3-Config";
    rw_s3_target_ph: "workspace id from the backup table · or molt/<id>/<ts>.molt.enc", "Workspace-ID aus der Backup-Tabelle · oder molt/<id>/<ts>.molt.enc";
    rw_via_file: "Manual restore", "Manuelles Restore";
    rw_file_pick: "Select", "Auswählen";
    rw_log_title: "Live details", "Live-Details";
    rw_finish: "Finish", "Fertigstellen";
    rw_failed: "Failed - see the live details", "Fehlgeschlagen - siehe Live-Details";
    // the honest §4.4 boundary: knowledge vs membership
    // origin-neutral on purpose: the engine derives "detached" from the
    // directory's state (no group key, no mesh), not from HOW it got there
    toast_detached: "Workspace is detached - rejoin via a recovery link.", "Workspace ist detached - Wiederbeitritt über Recovery-Link.";
    toast_reattaching: "Reconnecting to the republic…", "Verbinde wieder mit der Republik…";
    toast_backup_failed: "Backup failed:", "Backup fehlgeschlagen:";
    toast_backup_prune: "Backup stored, pruning old copies failed:", "Backup gespeichert, Aufräumen alter Kopien fehlgeschlagen:";
    // mesh self-heal Phase 4 — connection-health banner (net_health tone 1/2)
    banner_reconnecting: "Reconnecting…", "Verbinde erneut…";
    banner_disconnected: "Disconnected - you're not sending or receiving.", "Getrennt - du sendest und empfängst nichts.";
    banner_repair: "Repair connection", "Verbindung reparieren";
    banner_repair_tip: "Rejoin via a recovery link from a member who's online.", "Wiederbeitritt über einen Recovery-Link eines Mitglieds, das online ist.";
    banner_gap_note: "Messages sent while you were disconnected won't appear.", "Während der Trennung gesendete Nachrichten erscheinen nicht.";
    rw_ph1: "Connecting…", "Verbinde…";
    rw_ph2: "Fetching encrypted data…", "Lade verschlüsselte Daten…";
    rw_ph3: "Decrypting & verifying…", "Entschlüssele & prüfe…";
    set_title: "Settings", "Einstellungen";
    set_tab_general: "General", "Allgemein";
    set_tab_workspace: "Workspace", "Workspace";
    set_tab_backup: "Backup", "Backup";
    // the bucket's address and credentials — a separate errand from WHEN
    // and WHICH workspace is backed up, hence a separate tab
    set_tab_s3: "S3 config", "S3-Config";
    // the former single "Network" tab is split in two: the anonymity layer
    // (tor/none) and the Nostr relay pool — related, hence adjacent
    set_tab_anon: "Anonymity network", "Anonymitäts-Netzwerk";
    set_tab_relays: "Nostr relays", "Nostr-Relays";
    set_tab_mcp: "MCP", "MCP";
    set_tab_node: "Node", "Node";
    set_tab_chain: "Workspace chain history", "Workspace-Chain-History";
    chain_col_height: "Block", "Block";
    chain_col_what: "Change", "Änderung";
    chain_col_signers: "Signed by", "Signiert von";
    chain_kind_genesis: "Founding", "Gründung";
    chain_kind_membership: "Membership", "Mitgliedschaft";
    chain_kind_checkpoint: "Checkpoint (compacted)", "Checkpoint (kompaktiert)";
    chain_pre_cut: "before the cut", "vor dem Schnitt";
    chain_empty: "No chain - this workspace is not chain-governed.", "Keine Chain - dieser Workspace ist nicht chain-regiert.";
    set_ws_choose: "Choose folder…", "Ordner auswählen…";
    set_ws_dir_title: "Choose workspace folder", "Workspace-Ordner auswählen";
    set_ws_dir_body: "Path of the folder that holds your workspaces - browse via the file dialog or type it directly.", "Pfad des Ordners, der deine Workspaces enthält - per Datei-Dialog auswählen oder direkt eintippen.";
    set_ws_dir_browse: "Browse…", "Durchsuchen…";
    set_ws_found_one: "workspace found in this folder", "Workspace in diesem Ordner gefunden";
    set_ws_found_many: "workspaces found in this folder", "Workspaces in diesem Ordner gefunden";
    field_s3_access: "Access key", "Access-Key";
    field_s3_secret: "Secret key", "Secret-Key";
    field_s3_bucket: "Bucket", "Bucket";
    set_s3_test: "Test connection", "Verbindung testen";
    set_s3_active: "active", "aktiv";
    field_read_receipts: "Read receipts", "Lesebestätigungen";
    set_read_receipts: "Send read receipts", "Lesebestätigungen senden";
    set_s3_every: "every", "alle";
    set_s3_unit_min: "min", "Minuten";
    set_s3_keep: "save up to", "behalte bis zu";
    set_s3_unit_copies: "copies", "Kopien";
    s3_test_tip: "Sends a signed probe to the bucket over the configured transport - Tor when it is enabled.", "Sendet eine signierte Testanfrage an den Bucket über den konfigurierten Transport - via Tor, wenn aktiviert.";
    // one S3 account, several buckets (docs/storage/s3_buckets.md)
    set_s3_grp_account: "Endpoint & credentials", "Endpunkt & Zugangsdaten";
    set_s3_grp_backup: "Workspace backups", "Workspace-Backups";
    set_s3_grp_media: "Media", "Medien";
    set_s3_media_unused: "Stored only - nothing writes media here yet.", "Nur gespeichert - hierher schreibt noch nichts.";
    field_s3_max: "Limit (MiB)", "Limit (MiB)";
    set_s3_max_hint: "0 = no limit. Oldest copies go first, never a republic's last.", "0 = kein Limit. Älteste Kopien zuerst, nie die letzte einer Republik.";
    set_s3_max_hint_media: "0 = no limit.", "0 = kein Limit.";
    toast_backup_quota: "Backup stored, bucket quota:", "Backup gespeichert, Bucket-Limit:";
    s3_ok: "bucket reachable - credentials accepted ✓", "Bucket erreichbar - Zugangsdaten akzeptiert ✓";
    bk_col_local: "Local workspace", "Lokaler Workspace";
    bk_col_remote: "Backup in bucket", "Backup im Bucket";
    bk_col_auto: "Auto", "Auto";
    bk_col_size: "Size", "Größe";
    bk_col_last: "Last backup", "Letztes Backup";
    bk_refresh: "Refresh bucket", "Bucket aktualisieren";
    bk_refresh_tip: "Lists the saved bucket's backup objects over the configured transport - Tor when it is enabled. Backups without a local workspace appear as bucket-only rows.", "Listet die Backup-Objekte des gespeicherten Buckets über den konfigurierten Transport - via Tor, wenn aktiviert. Backups ohne lokalen Workspace erscheinen als Nur-Bucket-Zeilen.";
    bk_listing: "listing the bucket…", "Bucket wird gelesen…";
    // backup tab, when no bucket is configured yet: one line, one jump
    bk_needs_s3: "No S3 endpoint configured.", "Kein S3-Endpunkt konfiguriert.";
    bk_restore: "Restore", "Wiederherstellen";
    bkr_body: "The backup is downloaded, chain-verified and decrypted with this phrase; the workspace then opens on this device.", "Das Backup wird geladen, die Chain verifiziert und mit dieser Phrase entschlüsselt; der Workspace öffnet danach auf diesem Gerät.";
    bk_fetched_note: "Restored (still sealed) - open it with its recovery phrase", "Wiederhergestellt (noch versiegelt) - mit der Recovery-Phrase öffnen";
    bk_goto_open: "To the workspace list", "Zur Workspace-Liste";
    bk_list_ok: "bucket listed ✓", "Bucket gelesen ✓";
    set_save: "Save", "Speichern";
    set_save_note: "Saved to config.toml.", "In config.toml gespeichert.";
    set_close: "Close", "Schließen";
    set_path_label: "Config is written to", "Config wird geschrieben nach";
    set_reloaded_note: "config.toml changed on disk - settings reloaded.", "config.toml wurde auf der Platte geändert - Einstellungen neu geladen.";
    set_conflict_note: "config.toml on disk is invalid - the running settings stay. Fix the file or run --repair-config.", "config.toml auf der Platte ist ungültig - die laufenden Einstellungen bleiben. Datei korrigieren oder --repair-config ausführen.";
    set_restart_note: "Takes effect after a restart:", "Wirkt erst nach einem Neustart:";
    set_panel_appearance: "Language & appearance", "Sprache & Erscheinungsbild";
    set_panel_sounds: "Sound alerts", "Benachrichtigungstöne";
    field_sound_message: "New message", "Neue Nachricht";
    field_sound_vote: "New vote", "Neue Abstimmung";
    field_sound_poke: "Poke", "Anstupsen";
    sound_off: "off", "aus";
    field_poking: "Poking", "Anstupsen";
    set_poke_enabled: "Enable poking", "Anstupsen aktivieren";
    field_wake: "Agent wake", "Agenten wecken";
    wake_hint: "Runs on a poke or a vote waiting for you. Empty = off.", "Läuft bei einem Stups oder wartender Abstimmung. Leer = aus.";
    field_poke_wake: "Wake command", "Weck-Kommando";
    mem_poke: "Poke member", "Mitglied anstupsen";
    toast_poked: "poked you", "hat dich angestupst";
    toast_poke_sent: "Poked:", "Angestupst:";
    set_tor_embedded_missing: "\"embedded\" needs a build with --features embedded-tor - use a local Tor daemon instead.", "\"embedded\" braucht einen Build mit --features embedded-tor - nutze stattdessen einen lokalen Tor-Daemon.";
    // settings → Anonymity network: the Tor connectivity probe. The ladder's
    // rungs are worded so that NONE of them can be mistaken for a working Tor
    // except the last one — a listening SOCKS port proves a socket, not a
    // circuit (molt_core::TorTestState).
    set_tor_test: "Test Tor connection", "Tor-Verbindung testen";
    // kept short on purpose: HoverTip does not wrap, and the verdict line
    // under the button carries the full story anyway
    tor_test_tip: "Probes the draft above: the Tor SOCKS address, then a relay from your own pool through it.", "Prüft den Entwurf oben: die Tor-SOCKS-Adresse, dann ein Relay aus deinem eigenen Pool hindurch.";
    tor_v_idle: "Tor has not been tested yet.", "Tor wurde noch nicht getestet.";
    tor_v_testing: "testing Tor…", "teste Tor…";
    tor_v_off: "Nothing was sent - the anonymity network is not set to Tor.", "Es wurde nichts gesendet - das Anonymitäts-Netzwerk steht nicht auf Tor.";
    tor_v_misconfigured: "Nothing was probed: this Tor configuration was refused before a single packet. Fix it and test again.", "Es wurde nichts geprüft: Diese Tor-Konfiguration wurde abgelehnt, bevor ein einziges Paket lief. Korrigieren und erneut testen.";
    tor_v_no_proxy: "No Tor daemon: nothing is listening at this SOCKS address.", "Kein Tor-Daemon: An dieser SOCKS-Adresse lauscht nichts.";
    tor_v_proxy_only: "A Tor SOCKS port answers - but nothing was routed through it, so no circuit is proven. Add and confirm a relay to test a real circuit.", "Ein Tor-SOCKS-Port antwortet - aber es wurde nichts hindurchgeleitet, ein Circuit ist damit nicht bewiesen. Für einen echten Circuit-Test ein Relay hinzufügen und bestätigen.";
    tor_v_no_target: "Nothing could be established: this Tor mode has no SOCKS address to probe, and there was no relay to dial through it.", "Es konnte nichts festgestellt werden: Dieser Tor-Modus hat keine SOCKS-Adresse zum Prüfen, und es gab kein Relay, das hindurch gewählt werden konnte.";
    tor_v_circuit_failed: "No connection to the relay through Tor. Either Tor is not working, or that relay is unreachable - the line below says which step failed.", "Keine Verbindung zum Relay durch Tor. Entweder funktioniert Tor nicht, oder das Relay ist nicht erreichbar - die Zeile darunter sagt, welcher Schritt scheiterte.";
    tor_v_timeout: "No answer within the time limit. A first embedded-Tor start can take minutes - try again once it has bootstrapped.", "Keine Antwort innerhalb des Zeitlimits. Ein erster embedded-Tor-Start kann Minuten dauern - nach dem Bootstrap erneut versuchen.";
    tor_v_proxy_only_locked: "A Tor SOCKS port answers - but nothing was routed through it, so no circuit is proven. Your confirmed relays are not dialed: connections outside Tor are switched off.", "Ein Tor-SOCKS-Port antwortet - aber es wurde nichts hindurchgeleitet, ein Circuit ist damit nicht bewiesen. Deine bestätigten Relays werden nicht angewählt: Verbindungen außerhalb Tor sind ausgeschaltet.";
    tor_v_circuit: "Tor works: a relay from your own pool was reached end to end through Tor ✓", "Tor funktioniert: Ein Relay aus deinem eigenen Pool wurde Ende-zu-Ende durch Tor erreicht ✓";
    // settings → Nostr relays: the relay pool (docs_archive/transport/relay_pool.md §6).
    // The copy never promises a connection the policy does not make: an
    // added relay is idle, an onion relay connects by itself, a clearnet one
    // needs the warning AND the (persisted) non-onion dialing switch.
    rp_title: "Relay Pool", "Relay-Pool";
    rp_in_use: "Relays in use:", "Relays in Benutzung:";
    rp_none_dialable: "No relay is in use - this node is not connected.", "Kein Relay ist in Benutzung - dieser Knoten ist nicht verbunden.";
    rp_empty_title: "No relay configured yet", "Noch kein Relay eingerichtet";
    rp_empty_body: "This node is connected to nothing. Add a relay you trust and confirm it - .onion addresses are the private choice and connect on their own.", "Dieser Knoten ist mit nichts verbunden. Trag ein Relay ein, dem du vertraust, und bestätige es - .onion-Adressen sind die private Wahl und verbinden sich von selbst.";
    rp_badge_onion: "ONION", "ONION";
    rp_badge_clearnet: "CLEARNET", "CLEARNET";
    rp_badge_local: "LOCAL", "LOKAL";
    rp_st_auto: "connects automatically", "verbindet automatisch";
    rp_st_unconfirmed: "not in use - confirm to enable", "nicht in Benutzung - zum Aktivieren bestätigen";
    rp_st_locked: "confirmed - but clearnet/local dialing is switched off", "bestätigt - aber Clearnet-/Lokal-Verbindungen sind ausgeschaltet";
    rp_st_active: "in use", "in Benutzung";
    rp_confirm: "Confirm", "Bestätigen";
    rp_revoke: "Withdraw", "Zurückziehen";
    rp_revoke_tip: "Withdraw the confirmation - the relay stays in the list but is no longer used", "Bestätigung zurückziehen - das Relay bleibt in der Liste, wird aber nicht mehr benutzt";
    rp_copy: "Copy the address", "Adresse kopieren";
    rp_up: "Higher priority", "Höhere Priorität";
    rp_down: "Lower priority", "Niedrigere Priorität";
    rp_remove: "Remove from the list", "Aus der Liste entfernen";
    rp_add: "Add", "Hinzufügen";
    rp_add_hint: "Adding never connects: a new relay starts unconfirmed.", "Hinzufügen verbindet nicht: Ein neues Relay ist zunächst unbestätigt.";
    rp_err_scheme: "A relay address starts with wss:// (or ws:// for .onion and local addresses).", "Eine Relay-Adresse beginnt mit wss:// (oder ws:// bei .onion- und lokalen Adressen).";
    rp_err_host: "This address has no usable host.", "Diese Adresse hat keinen nutzbaren Host.";
    rp_err_plain: "ws:// is unencrypted - allowed for .onion and local addresses only, use wss:// here.", "ws:// ist unverschlüsselt - nur bei .onion- und lokalen Adressen erlaubt, hier brauchst du wss://.";
    rp_err_junk: "This address contains spaces or control characters.", "Diese Adresse enthält Leerzeichen oder Steuerzeichen.";
    rp_err_onion: "This is not a valid .onion address - a v3 onion has 56 characters (a–z, 2–7) before .onion.", "Das ist keine gültige .onion-Adresse - eine v3-Onion hat 56 Zeichen (a–z, 2–7) vor .onion.";
    rp_err_userinfo: "Credentials do not belong in a relay address.", "Zugangsdaten gehören nicht in eine Relay-Adresse.";
    rp_err_fragment: "A relay address cannot carry a #fragment.", "Eine Relay-Adresse kann kein #Fragment tragen.";
    rp_err_toolong: "This address is too long (max. 512 characters).", "Diese Adresse ist zu lang (max. 512 Zeichen).";
    rp_err_noncanon: "Write the address plainly - host, IP and port in their canonical form.", "Schreib die Adresse schlicht - Host, IP und Port in ihrer kanonischen Form.";
    rp_err_dup: "This relay is already in the list.", "Dieses Relay steht schon in der Liste.";
    rp_cn_title: "Use a clearnet relay?", "Ein Clearnet-Relay benutzen?";
    rp_cn_title_local: "Use a local relay?", "Ein lokales Relay benutzen?";
    rp_cn_body_tor: "Not a .onion service: its operator sees what this node subscribes to and when it is online. Tor hides your IP address - the endpoint stays in someone else's hands.", "Kein .onion-Dienst: Sein Betreiber sieht, was dieser Knoten abonniert und wann er online ist. Tor verbirgt deine IP-Adresse - der Endpunkt bleibt in fremder Hand.";
    rp_cn_body_plain: "Not a .onion service: its operator sees your IP address, what this node subscribes to and when it is online. Tor is off, so nothing hides where you connect from.", "Kein .onion-Dienst: Sein Betreiber sieht deine IP-Adresse, was dieser Knoten abonniert und wann er online ist. Tor ist aus, nichts verbirgt, von wo du dich verbindest.";
    rp_cn_body_local: "This relay is on your machine or local network - reached directly, Tor is not involved. Whoever runs it still sees what this node subscribes to and when it is online, and a ws:// address is readable along the local path.", "Dieses Relay liegt auf deinem Rechner oder lokalen Netz - es wird direkt erreicht, ohne Tor. Wer es betreibt, sieht trotzdem, was dieser Knoten abonniert und wann er online ist, und eine ws://-Adresse ist auf dem lokalen Weg mitlesbar.";
    rp_cn_ack: "I understand this and want to use the relay.", "Ich habe das verstanden und will das Relay benutzen.";
    rp_cn_confirm: "Confirm relay", "Relay bestätigen";
    rp_cn_note: "Confirming also switches connections outside Tor on and remembers that. You can switch them off again below at any time.", "Das Bestätigen schaltet Verbindungen außerhalb Tor zugleich ein und merkt sich das. Du kannst sie unten jederzeit wieder ausschalten.";
    rp_cn_session_title: "Relays outside Tor", "Relays außerhalb Tor";
    rp_cn_session_off: "Switched off: confirmed clearnet and local relays are not dialed at all - founding and joining over one is refused. Switching it on is remembered.", "Ausgeschaltet: Bestätigte Clearnet- und lokale Relays werden gar nicht angewählt - Gründen und Beitreten über so eines wird abgelehnt. Das Einschalten wird gemerkt.";
    rp_cn_session_on: "On: confirmed clearnet and local relays are in use. This stays on until you switch it off.", "An: Bestätigte Clearnet- und lokale Relays werden benutzt. Das bleibt so, bis du es ausschaltest.";
    rp_cn_activate: "Switch on", "Einschalten";
    rp_cn_deactivate: "Switch off", "Ausschalten";
    unsaved_title: "Unsaved changes", "Ungespeicherte Änderungen";
    unsaved_body: "You changed settings without saving them. Save them to config.toml, or discard the edits?", "Du hast Einstellungen geändert, aber nicht gespeichert. In die config.toml speichern oder die Änderungen verwerfen?";
    unsaved_save: "Save & continue", "Speichern & weiter";
    unsaved_discard: "Discard & continue", "Verwerfen & weiter";
    unsaved_cancel: "Cancel", "Abbruch";
    mv_send: "Send", "Senden";
    mv_propose: "Propose", "Vorschlagen";
    mv_approve: "Approve", "Zustimmen";
    mv_decline: "Decline", "Ablehnen";
    mv_pending: "Pending decisions", "Offene Entscheidungen";
    mv_declined: "Declined proposals", "Abgelehnte Vorschläge";
    mv_empty_declined: "No declined proposals right now - this view empties on the chat retention rhythm.", "Gerade keine abgelehnten Vorschläge - diese Ansicht leert sich im Chat-Aufbewahrungsrhythmus.";
    pc_declined_by: "Declined by", "Abgelehnt von";
    mv_applied: "Applied", "Angewandt";
    mv_accepted: "Accepted changes", "Angenommene Änderungen";
    // the decided-votes table (Accepted/Declined overviews)
    dt_col_decision: "Decision", "Entscheidung";
    dt_col_value: "Value", "Wert";
    dt_col_votes: "Votes", "Stimmen";
    dt_col_when: "When", "Wann";
    // error-toast prefixes (i18n audit 2026-08-09)
    toast_file_unreadable: "Cannot read:", "Nicht lesbar:";
    toast_relay_refused: "Relay refused:", "Relay abgelehnt:";
    toast_relay_unverified: "Relay not verifiable right now:", "Relay derzeit nicht prüfbar:";
    toast_checkpoint_sealed: "Checkpoint sealed", "Checkpoint besiegelt";
    mv_chat_ph: "Write a message…", "Nachricht schreiben…";
    mv_propose_ph: "Describe a proposal…", "Vorschlag beschreiben…";
    mv_empty_chat: "No messages yet.", "Noch keine Nachrichten.";
    mv_empty_pending: "Nothing awaiting approval.", "Nichts wartet auf Zustimmung.";
    mv_empty_applied: "Nothing applied yet.", "Noch nichts angewandt.";
    mv_deleted_by: "deleted by", "gelöscht durch";
    ch_discussions: "Discussions", "Diskussionen";
    ch_group: "General", "Allgemein";
    ch_new_topic: "New topic", "Neues Thema";
    ch_topic_ph: "Topic name…", "Themenname…";
    ch_topic_open: "Open topic", "Thema öffnen";
    ch_to_vote: "To the vote", "Zur Abstimmung";
    mv_file_gone: "File no longer available - its owner deleted it.", "Datei nicht mehr verfügbar - der Besitzer hat sie gelöscht.";
    toast_dl_done: "Saved:", "Gespeichert:";
    toast_dl_failed: "Download failed:", "Download fehlgeschlagen:";
    toast_file_removed: "Local file deleted - the share is no longer available.", "Lokale Datei gelöscht - die Freigabe ist nicht mehr verfügbar.";
    dm_title: "Delete message?", "Nachricht löschen?";
    dm_body: "The text disappears for everyone and only a deletion notice remains - a replicated tombstone addressed by message id, kept in the event log.", "Der Text verschwindet für alle, nur ein Lösch-Hinweis bleibt - ein replizierter Tombstone, per Nachrichten-ID adressiert und im Event-Log gehalten.";
    dm_confirm: "Delete", "Löschen";
    mv_close_ws: "Close workspace", "Workspace schließen";
    close_ws_title: "Close workspace?", "Workspace schließen?";
    close_ws_body: "You'll return to the start screen. Closing cleanly persists the group and transport state, so reopening resumes the live mesh where you left off.", "Du kehrst zum Startbildschirm zurück. Ein sauberes Schließen sichert den Gruppen- und Transport-Zustand, sodass das erneute Öffnen das Live-Mesh dort fortsetzt, wo du aufgehört hast.";
    close_ws_confirm: "Close workspace", "Workspace schließen";
    close_ws_cancel: "Cancel", "Abbrechen";
    tip_theme: "Theme", "Theme";
    tip_language: "Language", "Sprache";
    tip_settings: "Settings", "Einstellungen";
    quit_title: "Quit MoltRepublic?", "MoltRepublic beenden?";
    quit_body: "A workspace is open. Quitting shuts the node down; the GUI and its MCP endpoint stop.", "Ein Workspace ist offen. Beenden fährt den Node herunter; GUI und MCP-Endpoint stoppen.";
    quit_confirm: "Quit", "Beenden";
    // surface design mocks (Memory / Quests / Vault / Wallet panes): badged
    // UX drafts — the sample data stays .slint-side, only chrome localizes
    mock_badge: "DESIGN MOCK", "DESIGN-MOCK";
    mock_tip: "A design draft with sample data - nothing here is stored, sent, or real.", "Ein Design-Entwurf mit Beispieldaten - nichts hier wird gespeichert, gesendet oder ist echt.";
    mem_title_brain: "Multisig-Wiki", "Multisig-Wiki";
    mem_tb_new_file: "New file", "Neue Datei";
    mem_tb_new_folder: "New folder", "Neuer Ordner";
    mem_tb_delete: "Delete", "Löschen";
    mem_tb_collapse: "Collapse all", "Alles einklappen";
    mem_tb_open_all: "Expand all", "Alles ausklappen";
    mem_tb_edit: "Edit as Markdown", "Als Markdown bearbeiten";
    mem_tb_preview: "Preview", "Vorschau";
    mem_tb_link: "Copy path", "Pfad kopieren";
    mem_tb_locate: "Reveal in navigator", "Im Navigator zeigen";
    mem_tb_prev: "Previous tab", "Vorheriger Tab";
    mem_tb_next: "Next tab", "Nächster Tab";
    mem_toast_link: "Path copied", "Pfad kopiert";
    mem_menu_open: "Open", "Öffnen";
    mem_menu_rename: "Rename", "Umbenennen";
    mem_menu_delete: "Delete", "Löschen";
    mem_menu_move_root: "Move to root", "In die oberste Ebene";
    pc_withdrawn: "pulled back", "zurückgezogen";
    mem_menu_close_all: "Close all", "Alle schließen";
    mem_menu_close_right: "Close all to the right", "Alle rechts schließen";
    mem_menu_close_left: "Close all to the left", "Alle links schließen";
    mem_empty: "Nothing here yet - create a new file.", "Noch nichts hier - lege eine neue Datei an.";
    mem_linked: "Linked", "Verknüpft";
    mem_title_archive: "Archived notes", "Archivierte Notizen";
    mem_hint_archive: "Retired from the wiki - still readable, no longer linked.", "Aus dem Wiki zurückgezogen - weiter lesbar, nicht mehr verknüpft.";
    mem_cs_title: "Changeset", "Changeset";
    mem_cs_new: "new", "neu";
    mem_cs_deleted: "deleted", "gelöscht";
    mem_cs_moved: "moved", "verschoben";
    mem_cs_lines: "lines", "Zeilen";
    mem_cs_undo: "Undo", "Rückgängig";
    mem_cs_revert: "Discard all", "Alles verwerfen";
    mem_cs_vote: "Start vote", "Vote starten";
    mem_cs_confirm_title: "Discard all changes?", "Alle Änderungen verwerfen?";
    mem_cs_confirm_body: "Every local change is lost - this cannot be undone.", "Alle lokalen Änderungen gehen verloren - nicht rückgängig zu machen.";
    mem_toast_net_empty: "Changes cancel out - changeset cleared", "Änderungen heben sich auf - Changeset geleert";
    mem_menu_revert: "Discard changes", "Änderungen verwerfen";
    mem_menu_copy_link: "Copy link", "Link kopieren";
    mem_toast_link_md: "Link markup copied", "Link-Markup kopiert";
    mem_changed_hint: "changed", "geändert";
    mem_tb_revert_doc: "Discard this file's changes", "Änderungen dieser Datei verwerfen";
    mem_tb_export: "Export wiki", "Wiki exportieren";
    mem_ex_title: "Export wiki", "Wiki exportieren";
    mem_ex_body: "The approved wiki, as plain files.", "Das freigegebene Wiki, als einfache Dateien.";
    mem_ex_confirm: "Export", "Exportieren";
    mem_ex_proof: "Include verification bundle", "Prüfpaket beilegen";
    mem_ex_reveals: "Reveals member names, keys, transport anchors, relays, the charter and each patch's signers.", "Zeigt Mitgliedsnamen, Schlüssel, Transport-Anker, Relays, die Charta und die Unterzeichner jedes Patches.";
    mem_ex_drafts: "local changes stay local", "lokale Änderungen bleiben lokal";
    mem_ex_done: "wiki exported:", "Wiki exportiert:";
    mem_ex_file: "file", "Datei";
    mem_ex_files: "files", "Dateien";
    mem_ex_failed: "wiki export failed:", "Wiki-Export fehlgeschlagen:";
    ed_copy: "Copy", "Kopieren";
    ed_cut: "Cut", "Ausschneiden";
    ed_paste: "Paste", "Einfügen";
    pc_show_patch: "Raw patch", "Roher Patch";
    pc_withdraw: "Pull back", "Zurückziehen";
    pc_superseded: "superseded - base moved", "überholt - Basis hat sich bewegt";
    mem_rescue: "Rescue into working set", "Ins Working Set retten";
    mem_toast_rescued: "rescued", "gerettet";
    pv_title: "Wiki patch", "Wiki-Patch";
    pv_copy: "Copy patch", "Patch kopieren";
    pv_copied: "Patch copied", "Patch kopiert";
    kb_title_board: "Board", "Board";
    kb_hint_board: "The shared plan - every change on it is a threshold vote.", "Der gemeinsame Plan - jede Änderung daran ist eine Schwellen-Abstimmung.";
    kb_col_backlog: "Backlog", "Backlog";
    kb_col_ready: "Ready", "Bereit";
    kb_col_doing: "Doing", "In Arbeit";
    kb_col_review: "Review", "Review";
    kb_col_done: "Done", "Fertig";
    kb_title_plan: "Planning", "Planung";
    kb_hint_plan: "Sprints and dates - where the work is scheduled.", "Sprints und Termine - hier wird die Arbeit eingeplant.";
    kb_title_create: "Create", "Erstellen";
    kb_hint_create: "Draft an epic, story or task - proposing starts a vote.", "Entwirf ein Epic, eine Story oder einen Task - Vorschlagen startet eine Abstimmung.";
    kb_title_mine: "Mine", "Meine";
    kb_hint_mine: "Items you are responsible for.", "Einträge, für die du verantwortlich bist.";
    kb_title_archive: "Archive", "Archiv";
    kb_hint_archive: "Closed items - done or dropped.", "Geschlossene Einträge - fertig oder verworfen.";
    kb_kind_epic: "Epic", "Epic";
    kb_kind_story: "Story", "Story";
    kb_kind_task: "Task", "Task";
    kb_f_parent: "Parent", "Übergeordnet";
    kb_f_resp: "Responsible", "Verantwortlich";
    kb_f_start: "Start", "Start";
    kb_f_due: "Due", "Fällig";
    kb_f_points: "Points", "Punkte";
    kb_f_prio: "Priority", "Priorität";
    kb_f_sprint: "Sprint", "Sprint";
    kb_f_pi: "PI", "PI";
    kb_f_deps: "Depends on", "Hängt ab von";
    kb_f_refs: "Referenced by", "Verwiesen von";
    kb_sec_details: "Details", "Details";
    kb_sec_ready: "Definition of Ready", "Definition of Ready";
    kb_sec_done: "Acceptance criteria", "Akzeptanzkriterien";
    kb_sec_scope: "Out of scope", "Nicht enthalten";
    kb_prio_low: "Low", "Niedrig";
    kb_prio_normal: "Normal", "Normal";
    kb_prio_high: "High", "Hoch";
    kb_prio_critical: "Critical", "Kritisch";
    kb_ph_title: "Title", "Titel";
    kb_ph_parent: "#id - optional", "#id - optional";
    kb_ph_resp: "member", "Mitglied";
    kb_ph_date: "YYYY-MM-DD", "JJJJ-MM-TT";
    kb_ph_points: "3", "3";
    kb_ph_deps: "#id, #id", "#id, #id";
    kb_ph_details: "What and why - markdown, [links](notes/page.md) allowed", "Was und warum - Markdown, [Links](notes/page.md) erlaubt";
    kb_ph_ready: "Ready when: what must exist before work starts", "Bereit wenn: was vor Arbeitsbeginn vorliegen muss";
    kb_ph_done: "Done when: verifiable acceptance criteria", "Fertig wenn: prüfbare Akzeptanzkriterien";
    kb_ph_scope: "Explicitly not part of this item", "Ausdrücklich nicht Teil dieses Eintrags";
    kb_propose: "Propose", "Vorschlagen";
    kb_propose_change: "Propose change", "Änderung vorschlagen";
    kb_back: "Back", "Zurück";
    kb_mv_review: "Propose: move to review", "Vorschlagen: nach Review";
    kb_mv_done: "Propose: move to done", "Vorschlagen: nach Fertig";
    kb_rollup: "rolled up", "aufsummiert";
    kb_committed: "committed", "eingeplant";
    kb_current: "current", "aktuell";
    kb_unscheduled: "not scheduled", "nicht eingeplant";
    kb_closed_done: "done", "fertig";
    kb_closed_dropped: "dropped", "verworfen";
    vt_title_secrets: "Sealed secrets", "Versiegelte Geheimnisse";
    vt_hint_secrets: "Encrypted deposits - every seat guards one key share, below the threshold nothing opens.", "Verschlüsselte Einlagen - jeder Sitz hütet einen Schlüssel-Anteil, unterhalb der Schwelle öffnet nichts.";
    vt_seal_new: "Seal a secret", "Geheimnis versiegeln";
    vt_sealed_by: "sealed by", "versiegelt von";
    vt_opens_at: "opens at", "öffnet bei";
    vt_sealing: "sealing", "wird versiegelt";
    vt_verified_word: "verified", "geprüft";
    vt_title_requests: "Access requests", "Zugriffs-Anträge";
    vt_hint_requests: "A vote elects the one reader - at the threshold the key shares re-seal to that member alone.", "Ein Vote wählt den einen Leser - ab der Schwelle werden die Schlüssel-Anteile allein auf dieses Mitglied umversiegelt.";
    vt_request: "Request access", "Zugriff beantragen";
    vt_requested_by: "requested by", "beantragt von";
    vt_signed_word: "signed", "signiert";
    vt_resealed_word: "re-sealed", "umversiegelt";
    vt_granted_word: "granted", "erteilt";
    vt_denied_word: "denied", "abgelehnt";
    vt_title_unsealed: "Unsealed", "Entsiegelt";
    vt_hint_unsealed: "Every grant is on record - the content opens only for the elected reader.", "Jede Erteilung steht im Protokoll - der Inhalt öffnet sich nur für den gewählten Leser.";
    vt_only_you: "readable only by you", "nur für dich lesbar";
    vt_only_by: "readable only by", "lesbar nur für";
    vt_unsealed_word: "unsealed", "entsiegelt";
    vt_request_unseal: "Request unseal", "Beantragen";
    vt_seal_body: "Deposited encrypted - every seat guards one key share.", "Verschlüsselt hinterlegt - jeder Sitz hütet einen Schlüssel-Anteil.";
    vt_seal_confirm: "Seal", "Versiegeln";
    vt_f_title: "Title", "Titel";
    vt_f_title_ph: "Treasury seed phrase", "Treasury-Seed-Phrase";
    vt_f_desc: "Description", "Beschreibung";
    vt_f_desc_ph: "optional", "optional";
    vt_f_icon: "Icon", "Icon";
    vt_f_secret: "Secret", "Geheimnis";
    vt_f_secret_ph: "the text that gets sealed", "der Text, der versiegelt wird";
    vt_mock_note: "Mock - nothing was sent", "Mock - nichts gesendet";
    wl_title_balance: "Treasury balance", "Kassenstand";
    wl_hint_balance: "The shared Monero multisig wallet - no single member can spend from it.", "Die gemeinsame Monero-Multisig-Wallet - kein einzelnes Mitglied kann daraus ausgeben.";
    wl_unlocked: "unlocked", "verfügbar";
    wl_locked: "locked", "in Bestätigung";
    wl_rule_sample: "3-of-4 multisig", "3-von-4-Multisig";
    wl_pending_sigs: "Awaiting signatures", "Warten auf Signaturen";
    wl_title_history: "Transfers", "Transfers";
    wl_hint_history: "Every movement of the treasury, confirmations included.", "Jede Bewegung der Kasse, samt Bestätigungen.";
    wl_title_send: "Send from the treasury", "Aus der Kasse senden";
    wl_hint_send: "A transfer is a threshold vote.", "Ein Transfer ist ein Threshold-Vote.";
    wl_to_address: "Recipient address", "Empfängeradresse";
    wl_amount: "Amount (XMR)", "Betrag (XMR)";
    wl_priority: "Priority", "Priorität";
    wl_prio_low: "Low", "Niedrig";
    wl_prio_normal: "Normal", "Normal";
    wl_prio_high: "High", "Hoch";
    wl_fee: "network fee", "Netzwerkgebühr";
    wl_propose_transfer: "Propose transfer", "Transfer vorschlagen";
    wl_title_receive: "Receive into the treasury", "In die Kasse empfangen";
    wl_hint_receive: "Deposits land in the treasury - visible to every member.", "Einzahlungen landen in der Kasse - sichtbar für jedes Mitglied.";
    wl_subaddress: "Shared subaddress", "Gemeinsame Subadresse";
    wl_title_settings: "Wallet settings", "Wallet-Einstellungen";
    wl_hint_settings: "This node's Monero connection - the signers are fixed.", "Die Monero-Anbindung dieses Nodes - der Signer-Kreis ist fest.";
    wl_node: "Monero node", "Monero-Node";
    wl_sync: "Sync height", "Sync-Höhe";
    wl_signer_set: "Signer set", "Signer-Kreis";
}
