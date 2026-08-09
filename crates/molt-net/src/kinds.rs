// SPDX-License-Identifier: GPL-3.0-or-later

//! The Nostr event kinds this transport allocates — one registry, so two work
//! packages cannot allocate the same number in parallel.
//!
//! Before this module the numbers lived as bare literals and two
//! module-private constants in three files, and the follow-up that ordered
//! this package listed FOUR kinds while the code already used five:
//! [`KIND_RITUAL`] (446) was allocated without appearing in any inventory.
//! That is the collision this registry exists to prevent, already half
//! happened.
//!
//! 443/444/445 are fixed by the Marmot/NIP-EE spec and 1059 by NIP-59; only
//! 446 is ours to choose. Adding a kind means adding it to [`ALL`] — the test
//! below refuses duplicates, so a re-used number fails the build rather than
//! the wire.

/// **443** — a member's MLS KeyPackage, published so a founder can add them.
///
/// Not published by this build: the founding ritual carries the KeyPackage
/// inside the gift-wrapped JoinRequest instead, so nothing has needed a
/// standalone 443 yet. Registered because the number IS spoken for.
pub const KIND_KEY_PACKAGE: u16 = 443;

/// **444** — the MLS Welcome that brings a new member into the group.
pub const KIND_WELCOME: u16 = 444;

/// **445** — a group message: the MLS ciphertext under the current `h` tag.
pub const KIND_GROUP: u16 = 445;

/// **446** — a ritual message (ours, not spec'd): the founding/recovery
/// choreography that runs BEFORE the group exists, wrapped in a 1059.
pub const KIND_RITUAL: u16 = 446;

/// **447** — a file chunk (ours): one sealed block of a shared file's
/// chunk series (`file_plane.rs`). Separate from 445 so the group-log
/// subscription never drains file bytes and a download can ask for exactly
/// the series' publish window.
pub const KIND_FILE_CHUNK: u16 = 447;

/// **1059** — NIP-59 gift wrap: the sealed envelope every ritual message
/// travels in, authored by a fresh ephemeral key.
pub const KIND_GIFT_WRAP: u16 = 1_059;

/// Every kind this transport allocates. Add here when you add a constant —
/// the duplicate check keys off this slice.
pub const ALL: &[(u16, &str)] = &[
    (KIND_KEY_PACKAGE, "KeyPackage"),
    (KIND_WELCOME, "Welcome"),
    (KIND_GROUP, "group message"),
    (KIND_RITUAL, "ritual message"),
    (KIND_FILE_CHUNK, "file chunk"),
    (KIND_GIFT_WRAP, "gift wrap"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// No two kinds share a number, and every constant is registered.
    ///
    /// The registry is only worth having if it is COMPLETE: a constant that
    /// exists but is missing from [`ALL`] is invisible to the very check that
    /// is supposed to catch the next collision.
    #[test]
    fn the_registry_has_no_duplicates_and_lists_every_kind() {
        let mut seen: Vec<u16> = Vec::new();
        for (k, what) in ALL {
            assert!(
                !seen.contains(k),
                "kind {k} is allocated twice — the second one is {what}"
            );
            seen.push(*k);
        }
        for k in [
            KIND_KEY_PACKAGE,
            KIND_WELCOME,
            KIND_GROUP,
            KIND_RITUAL,
            KIND_FILE_CHUNK,
            KIND_GIFT_WRAP,
        ] {
            assert!(seen.contains(&k), "kind {k} is not registered in ALL");
        }
        assert_eq!(seen.len(), 6, "a kind was added without a test update");
    }

    /// The spec-fixed numbers are what the spec says. A typo here is not a
    /// compile error anywhere — it is a republic that cannot talk to itself.
    #[test]
    fn the_spec_fixed_kinds_keep_their_numbers() {
        assert_eq!(KIND_KEY_PACKAGE, 443);
        assert_eq!(KIND_WELCOME, 444);
        assert_eq!(KIND_GROUP, 445);
        assert_eq!(KIND_GIFT_WRAP, 1059);
        // ours, but on the wire all the same: changing it strands every peer
        // running the old number
        assert_eq!(KIND_RITUAL, 446);
    }
}
