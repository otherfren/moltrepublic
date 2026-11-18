// SPDX-License-Identifier: GPL-3.0-or-later
//! The command gate (`docs/web_gui.md` §5.2). `Command` deserializes every
//! variant, the ones that speak AS someone else included, so the door
//! admits by name and refuses everything it does not know.

use std::collections::BTreeSet;

use molt_core::Command;

/// Commands MCP keeps internal that the browser window needs: the GUI's
/// own doors. Everything else on `molt_core::INTERNAL` stays shut.
pub const WEB_DOOR: [&str; 9] = [
    // a referenced file's bytes for an inline picture
    "read_upload_bytes",
    // the settings form's direct door for the wake hook; the same value
    // rides `patch_settings` (ADR-0007), so this adds no reach
    "set_wake_command",
    // the byte doors (`docs/web_gui.md` §7): a browser holds bytes and no
    // host path. None of them takes a path - an upload is addressed by
    // an id the engine minted, a read by a share id, a view's picture
    // key or nothing at all - so they add no reach over `share_file`,
    // `download_file` and `export_workspace`, which take any path
    "upload_begin",
    "upload_chunk",
    "upload_abort",
    "share_upload",
    "read_file_bytes",
    "read_image_bytes",
    "read_export_bytes",
];

/// The allow-list: every MCP tool's command plus [`WEB_DOOR`].
#[derive(Debug, Clone)]
pub struct Gate {
    allowed: BTreeSet<&'static str>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    /// Built once per listener.
    #[must_use]
    pub fn new() -> Self {
        let mut allowed: BTreeSet<&'static str> = molt_mcp::tools().iter().map(|t| t.command).collect();
        allowed.extend(WEB_DOOR);
        Gate { allowed }
    }

    /// Whether the door executes this command.
    #[must_use]
    pub fn admits(&self, cmd: &Command) -> bool {
        self.admits_name(<&'static str>::from(cmd))
    }

    /// Why the door will not execute this command; `None` when it will.
    #[must_use]
    pub fn refusal(&self, cmd: &Command) -> Option<&'static str> {
        if !self.admits(cmd) {
            return Some("not allowed here");
        }
        // an empty seat key closes MCP and this door with it: a browser
        // form holds the write-only secrets empty, so one client bug
        // would lock its own user out (docs/web_gui.md §6.5)
        let empties_seat_key = match cmd {
            Command::PatchSettings { patch } => patch.get("mcp_token").and_then(|v| v.as_str()) == Some(""),
            Command::SetNodePosture { posture } => posture.mcp_token.as_deref() == Some(""),
            _ => false,
        };
        empties_seat_key.then_some("empty seat key")
    }

    /// The same by variant name.
    #[must_use]
    pub fn admits_name(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command is classified: the door admits it, or it is on the
    /// internal list and not a web door. A new `Command` goes red here
    /// until someone decides.
    #[test]
    fn every_command_is_admitted_or_deliberately_refused() {
        let gate = Gate::new();
        let refused: BTreeSet<&str> =
            molt_core::INTERNAL.iter().copied().filter(|n| !WEB_DOOR.contains(n)).collect();
        for name in Command::variant_names() {
            assert_ne!(
                gate.admits_name(name),
                refused.contains(name),
                "{name}: must be admitted XOR deliberately refused"
            );
        }
    }

    #[test]
    fn the_web_door_only_opens_internal_commands() {
        let tools: BTreeSet<&str> = molt_mcp::tools().iter().map(|t| t.command).collect();
        for name in WEB_DOOR {
            assert!(molt_core::INTERNAL.contains(&name), "{name} is not internal");
            assert!(!tools.contains(name), "{name} is already a tool");
        }
    }

    /// The ones that speak as someone else, by name: no refactor of the
    /// lists may open them.
    #[test]
    fn nobody_speaks_as_someone_else() {
        let gate = Gate::new();
        for name in [
            "as_operator",
            "ui_publish",
            "reload_settings",
            "config_notice",
            // a peer, a ritual member, a transport outcome
            "net_join_requested",
            "net_seal_signed",
            "net_delivered",
            "net_poked",
            "net_join_sealed",
            "net_recover_announced",
        ] {
            assert!(Command::variant_names().contains(&name), "{name} is not a command");
            assert!(!gate.admits_name(name), "{name}");
        }
        assert!(!gate.admits_name("no_such_command"));
    }

    #[test]
    fn a_command_value_is_judged_by_its_variant() {
        let gate = Gate::new();
        assert!(gate.admits(&Command::ReadSession));
        // the window's link read is an MCP tool, so the door has it
        assert!(gate.admits(&Command::ClassifyLink { link: String::new() }));
        assert!(!gate.admits(&Command::UiPublish { snapshot: Default::default() }));
    }

    #[test]
    fn an_empty_seat_key_is_refused_a_real_one_is_not() {
        let gate = Gate::new();
        let patch = |v: serde_json::Value| Command::PatchSettings { patch: v };
        assert_eq!(gate.refusal(&patch(serde_json::json!({ "mcp_token": "" }))), Some("empty seat key"));
        assert_eq!(gate.refusal(&patch(serde_json::json!({ "mcp_token": "new" }))), None);
        assert_eq!(gate.refusal(&patch(serde_json::json!({ "mcp_read_token": "" }))), None, "revoking the read key is legitimate");
        assert_eq!(gate.refusal(&Command::ReadSession), None);
        // the GUI's real Save goes through the posture
        let posture = |key: Option<&str>| {
            let mut p = molt_core::NodePosture::of(&molt_core::SessionSettings::default());
            p.mcp_token = key.map(str::to_string);
            Command::SetNodePosture { posture: p }
        };
        assert_eq!(gate.refusal(&posture(Some(""))), Some("empty seat key"));
        assert_eq!(gate.refusal(&posture(Some("new"))), None);
        assert_eq!(gate.refusal(&posture(None)), None);
        assert_eq!(gate.refusal(&Command::UiPublish { snapshot: Default::default() }), Some("not allowed here"));
    }

    /// The GUI must be able to do everything it does: every `Command::X`
    /// `molt-ui` and its client crate `molt-ui-kit` construct is admitted,
    /// except the window's own publish (the browser does not publish,
    /// §5.5). And no web door stands open that the GUI does not use.
    #[test]
    fn every_command_the_gui_issues_is_admitted() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).expect("readable").flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "tests") {
                        walk(&p, out);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let squash = |s: &str| s.replace('_', "").to_lowercase();
        let by_squashed: std::collections::BTreeMap<String, &str> =
            Command::variant_names().iter().map(|n| (squash(n), *n)).collect();
        let mut files = Vec::new();
        // the kit holds the upload helper: the three upload steps are its
        for krate in ["molt-ui", "molt-ui-kit"] {
            walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(krate).join("src"), &mut files);
        }
        let gate = Gate::new();
        let mut seen = BTreeSet::new();
        for f in files {
            // whole files, inline test modules included: over-inclusion fails loudly
            let text = std::fs::read_to_string(&f).expect("readable");
            for part in text.split("Command::").skip(1) {
                let ident: String = part.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if ident.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    if let Some(name) = by_squashed.get(&squash(&ident)) {
                        seen.insert(*name);
                    }
                }
            }
        }
        assert!(seen.len() > 90, "the scan found only {} commands", seen.len());
        for door in WEB_DOOR {
            assert!(seen.contains(door), "{door}: a web door the GUI never uses");
        }
        for name in seen {
            if name != "ui_publish" {
                assert!(gate.admits_name(name), "the GUI issues {name}, the door refuses it");
            }
        }
    }
}
