// SPDX-License-Identifier: GPL-3.0-or-later
//! The branch diagnostics (`docs/chain/chain_rescue_plan.md` §8, dark
//! choice): the fork lines, the per-branch detail rows, the engine's
//! branch reasons and notices in the active language.

use molt_core::branch::{BranchStatusView, ForkView};

/// One row of a fork's detail table.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct BranchRowData {
    /// Index of the fork in [`fork_lines`].
    pub(crate) fork: i32,
    /// A branch's heading row (else one of its blocks).
    pub(crate) heading: bool,
    pub(crate) text: String,
    /// "" or what dropping the block cannot undo.
    pub(crate) flag: String,
}

fn short(hash: &str) -> &str {
    hash.get(..6).unwrap_or(hash)
}

/// One line per fork: where it forked and both heads, ours named.
pub(crate) fn fork_lines(lang: i32, status: &BranchStatusView) -> Vec<String> {
    status.forks.iter().map(|f| fork_line(lang, f)).collect()
}

fn fork_line(lang: i32, f: &ForkView) -> String {
    let heads: Vec<String> = f
        .branches
        .iter()
        .map(|b| {
            let who = match (lang, b.own) {
                (1, true) => "unser",
                (1, false) => "anderer",
                (_, true) => "ours",
                (_, false) => "other",
            };
            format!("{who} {} · {}", b.head_height, short(&b.head_hash))
        })
        .collect();
    let at = if lang == 1 { "Bei Block" } else { "At block" };
    format!("{at} {}: {}", f.base_height, heads.join(", "))
}

/// Every fork's detail table, flattened: a heading per branch, then its
/// blocks with their risk.
pub(crate) fn branch_rows(lang: i32, status: &BranchStatusView) -> Vec<BranchRowData> {
    let mut rows = Vec::new();
    for (i, f) in status.forks.iter().enumerate() {
        let fork = i32::try_from(i).unwrap_or(i32::MAX);
        for b in &f.branches {
            let who = match (lang, b.own) {
                (1, true) => "Unser Zweig",
                (1, false) => "Anderer Zweig",
                (_, true) => "Our branch",
                (_, false) => "Other branch",
            };
            let counts: Vec<String> = b.counts.iter().map(|(k, n)| format!("{n} {}", kinds_label(lang, k))).collect();
            let mut text = format!("{who}: {} · {} · {}", b.head_height, short(&b.head_hash), counts.join(", "));
            if b.irreversible > 0 {
                text.push_str(&format!(" · {} {}", b.irreversible, risk_label(lang, "irreversible")));
            }
            rows.push(BranchRowData { fork, heading: true, text, ..BranchRowData::default() });
            rows.extend(b.blocks.iter().map(|r| {
                let what = if r.surface.is_empty() { r.op.clone() } else { format!("{} {}", r.surface, r.op) };
                BranchRowData {
                    fork,
                    heading: false,
                    text: format!("{} · {} {}", r.height, kind_label(lang, &r.kind), what.trim()),
                    flag: risk_label(lang, &r.risk).to_string(),
                }
            }));
        }
    }
    rows
}

fn kind_label(lang: i32, kind: &str) -> &str {
    match (lang, kind) {
        (1, "applied") => "Beschluss",
        (1, "membership") => "Mitgliedschaft",
        (1, "checkpoint") => "Schnitt",
        (1, "branch") => "Zweigwahl",
        (_, "applied") => "decision",
        (_, "membership") => "membership",
        (_, "checkpoint") => "checkpoint",
        (_, "branch") => "branch choice",
        (_, other) => other,
    }
}

fn kinds_label(lang: i32, kind: &str) -> &str {
    match (lang, kind) {
        (1, "applied") => "Beschlüsse",
        (1, "membership") => "Mitgliedschaften",
        (1, "checkpoint") => "Schnitte",
        (1, "branch") => "Zweigwahlen",
        (_, "applied") => "decisions",
        (_, "membership") => "memberships",
        (_, "checkpoint") => "checkpoints",
        (_, "branch") => "branch choices",
        (_, other) => other,
    }
}

fn risk_label(lang: i32, risk: &str) -> &'static str {
    match (lang, risk) {
        (1, "irreversible") => "unumkehrbar",
        (1, "flagged") => "prüfen",
        (_, "irreversible") => "irreversible",
        (_, "flagged") => "check",
        _ => "",
    }
}

/// A branch refusal's reason in German; the English is the engine's.
pub(crate) fn reason_de(t: &str) -> String {
    let fixed = match t {
        "not available in this build" => "in diesem Build nicht verfügbar",
        "no chain yet" => "noch keine Kette",
        "branch abandoned" => "Zweig aufgegeben",
        "no second branch" => "kein zweiter Zweig",
        "below a cut" => "unter einem Schnitt",
        "amnesiac: this device was restored above the fork point" => {
            "amnesisch: dieses Gerät wurde über dem Gabelpunkt wiederhergestellt"
        }
        "dropped list differs" => "Liste der verworfenen Blöcke weicht ab",
        "dropped blocks not held here" => "verworfene Blöcke liegen hier nicht vor",
        "stale: the chain moved above the tip" => "veraltet: die Kette ist über die Spitze hinaus",
        "tip not on a branch of this fork" => "Spitze liegt auf keinem Zweig dieser Gabelung",
        "too many open choices" => "zu viele offene Wahlen",
        "no signing key on this seat" => "kein Signaturschlüssel auf diesem Sitz",
        "choice not saved" => "Wahl nicht gespeichert",
        "not a branch bundle" => "kein Zweig-Bündel",
        "another republic" => "andere Republik",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.to_string();
    }
    if let Some(rest) = t.strip_prefix("needs a majority of the members: ") {
        if let Some((m, n)) = rest.strip_suffix(" is not one").and_then(|r| r.split_once(" of ")) {
            return format!("braucht eine Mehrheit der Mitglieder: {m} von {n} ist keine");
        }
    }
    if let Some(h) = t.strip_prefix("already chose at ") {
        return format!("schon bei {h} gewählt");
    }
    if let Some(h) = t.strip_prefix("conflicts with the choice at ") {
        return format!("widerspricht der Wahl bei {h}");
    }
    if let Some(e) = t.strip_prefix("bundle: ") {
        return format!("Bündel: {e}");
    }
    t.to_string()
}

/// A branch notice as a toast: `(text, is_error)`; `None` = not one.
pub(crate) fn notice_toast(lang: i32, notice: &str) -> Option<(String, bool)> {
    let de = lang == 1;
    let reason = |e: &str| if de { reason_de(e) } else { e.to_string() };
    if let Some(dest) = notice.strip_prefix("branch-exported:") {
        let head = if de { "Beweise gespeichert:" } else { "Evidence saved:" };
        return Some((format!("{head} {dest}"), false));
    }
    if let Some(e) = notice.strip_prefix("branch-export-failed:") {
        let head = if de { "Beweise nicht gespeichert:" } else { "Evidence not saved:" };
        return Some((format!("{head} {}", reason(e)), true));
    }
    if let Some(rest) = notice.strip_prefix("branch-imported:") {
        let (runs, cards) = rest.split_once(':').unwrap_or((rest, "0"));
        return Some((
            if de {
                format!("Beweise gelesen: {runs} Zweige, {cards} Karten")
            } else {
                format!("Evidence read: {runs} branches, {cards} cards")
            },
            false,
        ));
    }
    if let Some(e) = notice.strip_prefix("branch-import-failed:") {
        let head = if de { "Beweise nicht gelesen:" } else { "Evidence not read:" };
        return Some((format!("{head} {}", reason(e)), true));
    }
    if let Some(rest) = notice.strip_prefix("branch-abandoned:") {
        let (h, by) = rest.split_once(':').unwrap_or((rest, ""));
        let by = by.replace(',', ", ");
        return Some((
            if de {
                format!("Zweig bei Block {h} aufgegeben: {by}")
            } else {
                format!("Branch at block {h} abandoned: {by}")
            },
            true,
        ));
    }
    if let Some(h) = notice.strip_prefix("branch-stale:") {
        return Some((
            if de {
                format!("Zweigwahl bei Block {h} ist veraltet")
            } else {
                format!("Branch choice at block {h} is out of date")
            },
            true,
        ));
    }
    None
}
