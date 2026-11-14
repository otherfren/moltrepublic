// SPDX-License-Identifier: GPL-3.0-or-later
//! The Files tables' status cell (`docs_archive/files/download_progress.md` §4.6):
//! one short label, two thin bars - this seat's download and the serving
//! seat's publishing - and the tooltip that says what both mean.

use molt_core::{DownloadView, ServeView};

use crate::labels::file_size_label;

/// What one status cell shows.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct StatusCell {
    pub(crate) label: String,
    /// 0 idle · 1 running · 2 done · 3 failed · 4 serving
    pub(crate) kind: i32,
    /// This seat's download, 0..=1 (< 0 = no bar).
    pub(crate) own: f32,
    /// The serving seat's publishing, 0..=1 (< 0 = no bar).
    pub(crate) sharer: f32,
    pub(crate) tip: String,
}

fn de(lang: i32) -> bool {
    lang == 1
}

fn share(done: u32, of: u32) -> f32 {
    if of == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let f = done as f32 / of as f32;
    f.clamp(0.0, 1.0)
}

/// `90 s`, `12 min`, `3 h 5 min`, `2 d 4 h`.
pub(crate) fn duration_label(secs: u64) -> String {
    match secs {
        0..=89 => format!("{secs} s"),
        90..=5_399 => format!("{} min", (secs + 30) / 60),
        5_400..=172_799 => format!("{} h {} min", secs / 3_600, (secs % 3_600) / 60),
        _ => format!("{} d {} h", secs / 86_400, (secs % 86_400) / 3_600),
    }
}

fn gate_word(lang: i32, gate: &str) -> String {
    match (de(lang), gate) {
        (false, "outbox") => "chat goes first".into(),
        (true, "outbox") => "Chat geht vor".into(),
        (false, "budget") => "relay budget".into(),
        (true, "budget") => "Relay-Budget".into(),
        (false, "daily") => "daily cap reached".into(),
        (true, "daily") => "Tageslimit erreicht".into(),
        _ => String::new(),
    }
}

fn sharer_line(lang: i32, s: &ServeView) -> String {
    let gate = gate_word(lang, &s.gate);
    let base = if de(lang) {
        format!("Teilender: {} von {} Teilen gesendet", s.published, s.of)
    } else {
        format!("Sharer: {} of {} pieces sent", s.published, s.of)
    };
    if gate.is_empty() {
        base
    } else {
        format!("{base} ({gate})")
    }
}

fn bars_legend(lang: i32, sharer: bool) -> &'static str {
    match (de(lang), sharer) {
        (false, false) => "Bar: your download.",
        (true, false) => "Balken: dein Download.",
        (false, true) => "Upper bar: your download. Lower bar: the sharer sending to the relays.",
        (true, true) => "Oberer Balken: dein Download. Unterer: der Teilende sendet an die Relays.",
    }
}

fn wait_line(lang: i32, wait: &str) -> String {
    match (de(lang), wait) {
        (false, "sharer") => "Asked the sharer, no answer yet.".into(),
        (true, "sharer") => "Teilenden angefragt, noch keine Antwort.".into(),
        (false, "sharer_offline") => "Sharer offline - starts when they are back.".into(),
        (true, "sharer_offline") => "Teilender offline - startet, sobald er zurück ist.".into(),
        (false, "relays") => "No piece for a while - waiting on the relays.".into(),
        (true, "relays") => "Länger kein Teil - wartet auf die Relays.".into(),
        (false, "pieces") => "Missing pieces requested again.".into(),
        (true, "pieces") => "Fehlende Teile erneut angefragt.".into(),
        _ => String::new(),
    }
}

fn failure_word(lang: i32, code: &str) -> &'static str {
    match (de(lang), code) {
        (false, "no_relay") => "No relay",
        (true, "no_relay") => "Kein Relay",
        (false, "write") => "Disk",
        (true, "write") => "Speicher",
        (false, "checksum") => "Checksum",
        (true, "checksum") => "Prüfsumme",
        (false, "changed") => "Changed",
        (true, "changed") => "Geändert",
        (false, "gone") => "Gone",
        (true, "gone") => "Entfernt",
        (false, "expired") => "Expired",
        (true, "expired") => "Abgelaufen",
        (false, _) => "Failed",
        (true, _) => "Fehler",
    }
}

/// The cell for one row: `d` = this seat's download, `serving` = this
/// seat publishing the share itself.
pub(crate) fn status_cell(lang: i32, d: Option<&DownloadView>, serving: Option<&ServeView>, now: u64) -> StatusCell {
    let none = -1.0;
    let Some(d) = d.filter(|d| d.phase != "cancelled" && !d.phase.is_empty()) else {
        return match serving {
            Some(s) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let pct = (share(s.published, s.of) * 100.0).round() as u32;
                let mut tip = sharer_line(lang, s).replace(if de(lang) { "Teilender" } else { "Sharer" }, if de(lang) { "Du" } else { "You" });
                tip.push('\n');
                tip.push_str(if de(lang) {
                    "Balken: deine Teile auf den Relays. Bleib online, bis er voll ist."
                } else {
                    "Bar: your pieces on the relays. Stay online until it is full."
                });
                StatusCell {
                    label: if de(lang) { format!("Sende {pct} %") } else { format!("Sending {pct} %") },
                    kind: 4,
                    own: none,
                    sharer: share(s.published, s.of),
                    tip,
                }
            }
            None => StatusCell { own: none, sharer: none, ..StatusCell::default() },
        };
    };
    let sharer_bar = d.sharer.as_ref().map_or(none, |s| share(s.published, s.of));
    let mut lines: Vec<String> = Vec::new();
    let cell = |label: String, kind: i32, own: f32, lines: Vec<String>| StatusCell {
        label,
        kind,
        own,
        sharer: if kind == 1 { sharer_bar } else { none },
        tip: lines.join("\n"),
    };
    match d.phase.as_str() {
        "requested" => {
            lines.push(if de(lang) { "Startet." } else { "Starting." }.to_string());
            cell("0 %".to_string(), 1, 0.0, lines)
        }
        "waiting" => {
            let offline = d.wait == "sharer_offline";
            let label = match (de(lang), offline) {
                (false, false) => "Waiting",
                (true, false) => "Wartet",
                (false, true) => "Offline",
                (true, true) => "Offline",
            };
            lines.push(wait_line(lang, if d.wait.is_empty() { "sharer" } else { &d.wait }));
            if d.asked > 0 {
                let ago = duration_label(now.saturating_sub(d.asked));
                lines.push(if de(lang) { format!("Zuletzt gefragt vor {ago}.") } else { format!("Last asked {ago} ago.") });
            }
            if let Some(s) = &d.sharer {
                lines.push(sharer_line(lang, s));
            }
            lines.push(bars_legend(lang, d.sharer.is_some()).to_string());
            cell(label.to_string(), 1, 0.0, lines)
        }
        "transferring" => {
            let own = share(d.held, d.of);
            if d.of > 0 {
                lines.push(if de(lang) {
                    format!("{} % - {} von {} Teilen", d.percent, d.held, d.of)
                } else {
                    format!("{} % - {} of {} pieces", d.percent, d.held, d.of)
                });
            }
            if d.size > 0 {
                let of = if de(lang) { "von" } else { "of" };
                lines.push(format!("{} {of} {}", file_size_label(d.bytes), file_size_label(d.size)));
            }
            if d.rate > 0 {
                let mut l = format!("{}/s", file_size_label(d.rate));
                if d.eta > 0 {
                    let left = duration_label(d.eta);
                    l.push_str(&if de(lang) { format!(" - noch ca. {left}") } else { format!(" - about {left} left") });
                }
                lines.push(l);
            }
            if d.last_piece > 0 {
                let ago = duration_label(now.saturating_sub(d.last_piece));
                lines.push(if de(lang) { format!("Letztes Teil vor {ago}.") } else { format!("Last piece {ago} ago.") });
            }
            let wait = wait_line(lang, &d.wait);
            if !wait.is_empty() {
                lines.push(wait);
            }
            if let Some(s) = &d.sharer {
                lines.push(sharer_line(lang, s));
            }
            lines.push(bars_legend(lang, d.sharer.is_some()).to_string());
            cell(format!("{} %", d.percent), 1, own, lines)
        }
        "verifying" => {
            lines.push(if de(lang) { "Alle Teile da, Prüfsumme wird geprüft." } else { "All pieces in, checking the checksum." }.to_string());
            cell(if de(lang) { "Prüfe" } else { "Verifying" }.to_string(), 1, 1.0, lines)
        }
        "done" => {
            lines.push(if de(lang) { format!("Gespeichert: {}", d.path) } else { format!("Saved: {}", d.path) });
            cell("\u{2713}".to_string(), 2, none, lines)
        }
        "failed" => {
            lines.push(d.error.clone());
            lines.push(if de(lang) { "Download startet neu." } else { "Download starts it again." }.to_string());
            cell(format!("\u{26a0} {}", failure_word(lang, &d.code)), 3, none, lines)
        }
        _ => StatusCell { own: none, sharer: none, ..StatusCell::default() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(phase: &str) -> DownloadView {
        DownloadView { phase: phase.into(), ..DownloadView::default() }
    }

    #[test]
    fn a_transfer_shows_percent_both_bars_and_a_tip_that_explains_them() {
        let d = DownloadView {
            held: 12,
            of: 40,
            percent: 30,
            bytes: 528_000,
            size: 1_760_000,
            rate: 2_900,
            eta: 425,
            last_piece: 970,
            sharer: Some(ServeView { published: 20, of: 43, gate: "daily".into(), at: 1 }),
            ..dl("transferring")
        };
        let c = status_cell(0, Some(&d), None, 1_000);
        assert_eq!((c.label.as_str(), c.kind), ("30 %", 1));
        assert!((c.own - 0.3).abs() < 1e-6 && (c.sharer - 20.0 / 43.0).abs() < 1e-6, "{c:?}");
        for part in ["12 of 40 pieces", "/s - about 7 min left", "Last piece 30 s ago", "20 of 43 pieces sent (daily cap reached)", "Lower bar"] {
            assert!(c.tip.contains(part), "{part} in {}", c.tip);
        }
        let de = status_cell(1, Some(&d), None, 1_000);
        assert!(de.tip.contains("Unterer") && de.tip.contains("noch ca."), "{}", de.tip);
        assert!(!c.tip.contains('\u{2014}') && !de.tip.contains('\u{2014}'), "no em dash");
    }

    #[test]
    fn waiting_names_an_offline_sharer_and_has_no_sharer_bar_without_a_report() {
        let d = DownloadView { wait: "sharer_offline".into(), asked: 400, ..dl("waiting") };
        let c = status_cell(0, Some(&d), None, 1_000);
        assert_eq!((c.label.as_str(), c.kind, c.own, c.sharer), ("Offline", 1, 0.0, -1.0));
        assert!(c.tip.contains("Sharer offline") && c.tip.contains("Last asked 10 min ago"), "{}", c.tip);
    }

    #[test]
    fn ends_and_the_sharers_own_row() {
        let failed = DownloadView { error: "no dialable relay".into(), code: "no_relay".into(), ..dl("failed") };
        let c = status_cell(0, Some(&failed), None, 0);
        assert_eq!((c.label.as_str(), c.kind, c.own), ("\u{26a0} No relay", 3, -1.0));
        assert!(c.tip.starts_with("no dialable relay"));
        let done = DownloadView { path: "/x/a.bin".into(), ..dl("done") };
        assert_eq!(status_cell(1, Some(&done), None, 0).tip, "Gespeichert: /x/a.bin");
        assert_eq!(status_cell(0, Some(&dl("requested")), None, 0).label, "0 %");
        let idle = status_cell(0, Some(&dl("cancelled")), None, 0);
        assert_eq!((idle.label.as_str(), idle.kind), ("", 0));
        let serving = ServeView { published: 5, of: 20, gate: "outbox".into(), at: 1 };
        let own = status_cell(0, None, Some(&serving), 0);
        assert_eq!((own.label.as_str(), own.kind, own.own), ("Sending 25 %", 4, -1.0));
        assert!(own.tip.contains("You: 5 of 20 pieces sent (chat goes first)") && own.tip.contains("Stay online"), "{}", own.tip);
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(duration_label(45), "45 s");
        assert_eq!(duration_label(425), "7 min");
        assert_eq!(duration_label(7_500), "2 h 5 min");
        assert_eq!(duration_label(200_000), "2 d 7 h");
    }
}
