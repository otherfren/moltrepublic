// SPDX-License-Identifier: GPL-3.0-or-later
//! Who at a seat wrote a chat message: an optional, unauthenticated label
//! (GitHub issue #5). Anyone holding the seat key can set any label, so it
//! is attribution between the operators of one seat, never an identity.

use serde::{Deserialize, Serialize};

/// Longest label, in chars.
pub const OPERATOR_LABEL_MAX: usize = 32;

/// The operator of a seat behind one message: a human or an AI agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatOperator {
    /// Free label, e.g. the agent's name ("" = unnamed).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// An AI agent wrote it (false = a human).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
}

impl ChatOperator {
    /// The operator a command asks for: trimmed, at most
    /// [`OPERATOR_LABEL_MAX`] chars, no control chars. An unnamed human is
    /// no operator at all (`None`).
    pub fn checked(label: &str, agent: bool) -> Result<Option<ChatOperator>, String> {
        let label = label.trim();
        if label.chars().count() > OPERATOR_LABEL_MAX {
            return Err(format!("operator: at most {OPERATOR_LABEL_MAX} chars"));
        }
        if label.chars().any(hidden) {
            return Err("operator: no control or invisible characters".to_string());
        }
        if label.is_empty() && !agent {
            return Ok(None);
        }
        Ok(Some(ChatOperator { label: label.to_string(), agent }))
    }

    /// The label as a peer may show it: a foreign message was never
    /// checked here, so control chars go and the length is capped.
    pub fn shown_label(&self) -> String {
        self.label
            .chars()
            .filter(|c| !hidden(*c))
            .take(OPERATOR_LABEL_MAX)
            .collect::<String>()
            .trim()
            .to_string()
    }
}

/// A char that is invisible or reorders or breaks the line around it.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{AD}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_trims_caps_and_drops_an_unnamed_human() {
        assert_eq!(ChatOperator::checked("  ", false), Ok(None));
        assert_eq!(
            ChatOperator::checked(" Claude ", true),
            Ok(Some(ChatOperator { label: "Claude".into(), agent: true }))
        );
        assert_eq!(
            ChatOperator::checked("", true),
            Ok(Some(ChatOperator { label: String::new(), agent: true }))
        );
        assert_eq!(
            ChatOperator::checked("Ana", false),
            Ok(Some(ChatOperator { label: "Ana".into(), agent: false }))
        );
        assert!(ChatOperator::checked(&"x".repeat(33), true).is_err());
        assert!(ChatOperator::checked(&"ä".repeat(32), true).is_ok(), "chars, not bytes");
        assert!(ChatOperator::checked("a\u{1b}[31m", true).is_err());
        for c in ['\u{202E}', '\u{2066}', '\u{200B}', '\u{FEFF}', '\u{2028}'] {
            assert!(ChatOperator::checked(&format!("a{c}b"), true).is_err(), "{:?}", c);
        }
    }

    #[test]
    fn a_foreign_label_is_shown_clean() {
        let op = ChatOperator { label: format!("a\u{7}\u{202E}b{}", "y".repeat(40)), agent: true };
        let shown = op.shown_label();
        assert!(shown.starts_with("ab"));
        assert_eq!(shown.chars().count(), OPERATOR_LABEL_MAX);
    }

    #[test]
    fn the_wire_shape_is_compact() {
        let op = ChatOperator { label: "Claude".into(), agent: true };
        assert_eq!(serde_json::to_string(&op).expect("json"), r#"{"label":"Claude","agent":true}"#);
        let human = ChatOperator { label: "Ana".into(), agent: false };
        assert_eq!(serde_json::to_string(&human).expect("json"), r#"{"label":"Ana"}"#);
        assert!(!serde_json::from_str::<ChatOperator>("{}").expect("json").agent);
    }
}
