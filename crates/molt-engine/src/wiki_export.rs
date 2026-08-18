// SPDX-License-Identifier: GPL-3.0-or-later

//! **The wiki export** (`docs/memory/wiki_export_plan.md`): the Shared-Memory
//! tree written to a user-picked directory as plain files, optionally with the
//! threshold signatures that make it verifiable by an outsider — no moltd, no
//! workspace key, no trust in the exporter.
//!
//! What leaves the workspace is exactly two things: the folded wiki tree, and
//! the blocks that AUTHENTICATE it (the genesis, every `Membership` block, and
//! every applied `wiki_patch`). No other block kind is exported, so no other
//! surface's content rides along.
//!
//! The verifier is [`crate::verify_wiki_export`], beside `verify_chain` — it
//! reuses the real byte layouts, so writer and verifier cannot drift.

use std::path::{Component, Path, PathBuf};

use molt_core::{ChainBlock, ChainChange, Surface};
use serde::{Deserialize, Serialize};

/// The bundle's format tag. A verifier that meets an unknown one stops.
pub const WIKI_EXPORT_FORMAT: &str = "molt-wiki-export-v1";

/// `<dest>/proof/bundle.json`: the genesis plus the blocks a reviewer needs to
/// check every exported patch. Additive-only, like every wire shape here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiExportBundle {
    /// [`WIKI_EXPORT_FORMAT`].
    pub format: String,
    /// Block 0: the sealed founding constitution (n-of-n).
    pub genesis: ChainBlock,
    /// Every `Membership` block and every applied `wiki_patch`, ascending by
    /// height. Membership blocks ride along because they are the identity
    /// history: a later patch signed by a seat that joined after the founding
    /// verifies only against the roster those blocks establish.
    #[serde(default)]
    pub blocks: Vec<ChainBlock>,
}

/// Whether a change is an applied wiki patch (Memory surface, `wiki_patch`
/// op) — the ONE predicate the writer's selection and the verifier's
/// admission check share, so a block kind cannot be exported that the
/// verifier would refuse (or the reverse).
pub(crate) fn is_wiki_patch(change: &ChainChange) -> bool {
    matches!(
        change,
        ChainChange::Applied { surface: Surface::Memory, payload, .. }
            if payload.get("op").and_then(serde_json::Value::as_str) == Some("wiki_patch")
    )
}

/// Select the proof bundle out of a verified chain. `None` when the chain does
/// not start at its genesis (an empty chain, or a holder pruned to a
/// checkpoint anchor) — there is then nothing to anchor the signatures in, and
/// a bundle without that anchor would prove nothing.
pub(crate) fn bundle_from_chain(chain: &[ChainBlock]) -> Option<WikiExportBundle> {
    let genesis = chain.first()?;
    if !matches!(genesis.change, ChainChange::Genesis { .. }) {
        return None;
    }
    let blocks = chain
        .iter()
        .skip(1)
        .filter(|b| matches!(b.change, ChainChange::Membership { .. }) || is_wiki_patch(&b.change))
        .cloned()
        .collect();
    Some(WikiExportBundle {
        format: WIKI_EXPORT_FORMAT.to_string(),
        genesis: genesis.clone(),
        blocks,
    })
}

/// What one export actually wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WikiExportOutcome {
    /// Wiki documents written.
    pub files: u64,
    /// Total bytes written, the proof files included.
    pub bytes: u64,
}

/// A written length as the report's `u64` (a `usize` always fits on the
/// platforms this runs on; the saturating fallback keeps the count honest
/// rather than panicking over a display number).
fn byte_count(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// One document path, re-validated at the WRITE (never trust the fold to have
/// stayed the only producer): the fold's own rule — relative, at most eight
/// segments, no empty/dot/backslash segment — plus the platform check that
/// every segment is exactly one plain path component. A segment that any
/// platform reads as a root, a prefix or a parent link is refused, so the
/// joined path cannot leave `<dest>/wiki`.
fn safe_relative(path: &str) -> Result<PathBuf, String> {
    if !molt_core::wiki_fold::valid_path(path) {
        return Err(format!("unsafe path: {path}"));
    }
    let mut out = PathBuf::new();
    for segment in path.split('/') {
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(c)), None) if c == std::ffi::OsStr::new(segment) => {
                out.push(c);
            }
            _ => return Err(format!("unsafe path: {path}")),
        }
    }
    Ok(out)
}

/// Refuse a target whose path crosses a SYMLINK below `<dest>`: writing
/// through one puts wiki content into a file the user never picked, which is
/// exactly the escape [`safe_relative`] exists to prevent — a planted link
/// gets there without a single `..`. `<dest>` itself is the user's own choice
/// and may legitimately be a link; everything the export creates under it may
/// not be one.
fn check_no_symlink(dest: &Path, rel: &Path) -> Result<(), String> {
    let mut at = dest.to_path_buf();
    for component in rel.components() {
        at.push(component);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!("{}: symlink in the target path", rel.display()));
        }
    }
    Ok(())
}

/// Write the export: `<dest>/wiki/<path>` per document and, with a bundle,
/// `<dest>/proof/bundle.json` + `<dest>/proof/README.md`. Blocking — the
/// caller runs it off the actor.
///
/// Existing files of the same name are overwritten; nothing is deleted. A
/// leftover file from an earlier export therefore stays, and the verifier
/// says so (it compares the shipped tree against the fold, both ways).
pub(crate) fn write_wiki_export(
    dest: &Path,
    tree: &std::collections::BTreeMap<String, String>,
    bundle: Option<&WikiExportBundle>,
) -> Result<WikiExportOutcome, String> {
    let mut out = WikiExportOutcome::default();
    let wiki = dest.join("wiki");
    for (path, content) in tree {
        let rel = Path::new("wiki").join(safe_relative(path)?);
        let file = dest.join(&rel);
        if !file.starts_with(&wiki) {
            return Err(format!("unsafe path: {path}"));
        }
        check_no_symlink(dest, &rel)?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("wiki/{path}: {e}"))?;
        }
        std::fs::write(&file, content).map_err(|e| format!("wiki/{path}: {e}"))?;
        out.files += 1;
        out.bytes = out.bytes.saturating_add(byte_count(content.len()));
    }
    if let Some(bundle) = bundle {
        let proof = dest.join("proof");
        std::fs::create_dir_all(&proof).map_err(|e| format!("proof: {e}"))?;
        let json = serde_json::to_string_pretty(bundle)
            .map_err(|e| format!("proof/bundle.json: {e}"))?;
        check_no_symlink(dest, Path::new("proof/bundle.json"))?;
        std::fs::write(proof.join("bundle.json"), &json)
            .map_err(|e| format!("proof/bundle.json: {e}"))?;
        check_no_symlink(dest, Path::new("proof/README.md"))?;
        std::fs::write(proof.join("README.md"), PROOF_README)
            .map_err(|e| format!("proof/README.md: {e}"))?;
        out.bytes = out
            .bytes
            .saturating_add(byte_count(json.len()))
            .saturating_add(byte_count(PROOF_README.len()));
    }
    Ok(out)
}

/// Read an export back: the raw `proof/bundle.json` and the shipped `wiki/`
/// tree, keyed by `/`-joined relative path. The I/O half of
/// [`crate::verify_wiki_export`] — what the example binary and any other
/// reviewer's tool needs before the pure check runs.
pub fn read_wiki_export(
    dir: &Path,
) -> Result<(String, std::collections::BTreeMap<String, String>), String> {
    let bundle = dir.join("proof").join("bundle.json");
    let json = std::fs::read_to_string(&bundle).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("{}: no proof bundle in this export", dir.display())
        }
        _ => format!("{}: {e}", bundle.display()),
    })?;
    let mut tree = std::collections::BTreeMap::new();
    read_tree(&dir.join("wiki"), "", 0, &mut tree)?;
    Ok((json, tree))
}

/// The `wiki/` walk. The entry's OWN file type decides, never the target's:
/// symlinks and device nodes are skipped rather than followed (a FIFO here
/// would otherwise hang the reviewer's verifier forever) and the depth is
/// capped. A skipped entry is not silently forgiven — the tree then differs
/// from the fold, which is a failure.
fn read_tree(
    dir: &Path,
    prefix: &str,
    depth: usize,
    out: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    if depth > 16 {
        return Err(format!("{}: nested too deep", dir.display()));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(|e| format!("{path}: {e}"))?;
        if kind.is_dir() {
            read_tree(&entry.path(), &path, depth + 1, out)?;
        } else if kind.is_file() {
            let content = std::fs::read_to_string(entry.path())
                .map_err(|e| format!("{path}: {e}"))?;
            out.insert(path, content);
        }
    }
    Ok(())
}

/// `<dest>/proof/README.md`: the external reviewer's document. It states the
/// algorithm, the exact byte layouts (so an independent implementation is
/// possible without this code), what the bundle necessarily discloses, and
/// what it does NOT prove.
const PROOF_README: &str = r#"# Verifying this wiki export

`wiki/` is the republic's Shared-Memory tree. `proof/bundle.json` carries the
signatures that prove it: every file here is the deterministic fold of patches
that a threshold (m-of-n) of the republic's sealed roster approved.

Verifying needs no republic membership, no key, and no trust in whoever handed
you this directory.

## Run the reference verifier

    cargo run -p molt-engine --example verify_wiki_export -- <this directory>

It prints a verdict per step and exits non-zero on any failure. The sections
below specify the same check exactly enough to reimplement it.

## What the bundle contains

    { "format": "molt-wiki-export-v1",
      "genesis": <block 0>,
      "blocks":  [ <block>, ... ] }

`blocks` holds every membership block and every applied wiki patch, ascending
by height. It is a SUBSET of the republic's chain: other block kinds (and
their content) are not exported, so the usual `prev` hash links and contiguous
heights are absent by construction. That costs nothing, because every member
signature is position-bound: it covers the block's height, so each block
stands on its own against the roster valid at that height.

Each block is `{ height, prev, change, sigs }`; `sigs` is a list of
`{ member, sig }` with a lowercase-hex Ed25519 signature. `prev` is not part
of any signature and is not checked here.

## The check

1. **Genesis.** `change` is the founding table (`kind: "genesis"`): name,
   republic_id, rule_m, rule_n, identities, agenda, relays, optional features.
   Re-derive `republic_id` from the content (below) and require equality.
   Require `0 < rule_m <= rule_n`, `rule_n == identities.len()`, and a valid
   signature from EVERY identity over the genesis bytes (n-of-n).
2. **Roster walk.** Walk `blocks` in order, requiring strictly ascending
   heights. A `kind: "membership"` block with op `joined` appends its
   `(member, identity_pk)` to the roster; op `restored` must repeat the
   member's already anchored `identity_pk` (a recovery re-keys the transport,
   never the roster identity) and changes the roster in no other way.
3. **Every block.** Count the DISTINCT roster members with a valid signature
   over the block's bytes (below); an unknown signer, a bad signature and a
   repeated member all count once or not at all. On a `restored` block the
   returning member's own `consent` counts as one further signer if it
   verifies against its anchored key over the consent bytes and that member
   does not also appear in `sigs`. Require at least `rule_m`. Refuse any block
   that is neither a membership block nor an applied wiki patch, and refuse a
   proposal id that appears twice.
4. **Fold.** Apply the patches in height order (below) and compare the result
   with `wiki/` byte for byte, in both directions: a missing file, an extra
   file and a changed byte all fail.

## Byte layouts

Framing: `le32(n)` is a 4-byte little-endian length, `le64(n)` an 8-byte one.
`F(x)` means `le32(len(x))` followed by the UTF-8 bytes of `x`. Tags include
their trailing NUL byte. Signatures are Ed25519 (strict verification) over the
byte string, with keys and signatures as lowercase hex.

**Block bytes** (what each `sigs` entry signs), for an applied change:

    "molt-chain-change-v2\0" F(republic_id) le64(height) 0x01
    le64(proposal_id) F(surface) F(payload)

`surface` is `memory` for a wiki patch. `payload` is the JSON object
serialized canonically: no whitespace, object keys sorted ascending by byte.

For a membership change:

    "molt-chain-change-v2\0" F(republic_id) le64(height) 0x02
    op F(member) F(identity_pk) nostr relays consent

with `op` = `0x00` joined / `0x01` restored; `nostr` = `0x00` when absent,
else `0x01 F(nostr_pk)`; `relays` is EMPTY when the block declares none, else
`0x01 le64(count)` followed by `F(relay)` per entry; `consent` is EMPTY when
absent, else `0x02 F(consent)`.

**Genesis bytes** are the founding roster table:

    "molt-roster-v4\0"          (no feature set)
    "molt-roster-v5\0"          (feature set present)
    F(republic_id) rule_m rule_n le32(count)
    per identity in table order: F(member) F(identity_pk) F(nostr_pk)
    F(agenda) le32(relay count) per relay: F(relay)
    [ le32(feature count) per feature: F(feature) ]

`rule_m` and `rule_n` are single bytes. The feature run is written only when
the genesis carries one, which is also what selects the tag.

**republic_id** is the lowercase hex SHA-256 of

    "molt-republic-id-v2\0" F(name) rule_m rule_n le32(count)
    per pair: F(identity_pk) F(nostr_pk)

over the `(identity_pk, nostr_pk)` pairs of the genesis identities, sorted
ascending as byte strings (identity_pk first, nostr_pk as tie-break).

**Consent bytes** (a restored seat's own co-signature):

    "molt-restore-consent-v1\0" F(republic_id) F(member) F(identity_pk)
    0x00                        (empty transport anchor)
    0x01 F(nostr_pk)            (otherwise)

## The fold

Each wiki patch payload is `{"op":"wiki_patch","value":<git-format patch>}`.
Apply the values in height order, starting from an empty tree, with a STRICT
apply: hunks must match at their exact old-side line positions and with exact
context, and either a whole patch applies or none of it does. A patch that
does not apply is void and is skipped - it moves nothing. Paths are relative,
at most eight segments, without empty, `.` or `..` segments.

## What this discloses

The bundle necessarily reveals the republic's name, charter, feature set,
member names, their Ed25519 identity keys, their transport anchors, the
ratified relay pool, and which members signed each patch. None of it is
redactable: those bytes are what the signatures cover.

## What this does NOT prove

**Completeness.** The signatures prove that every file here comes from patches
the republic really approved - not that they are ALL of them, and not that
they are the latest ones. Whoever produced this export could have left patches
out and shipped an older or partial, but entirely genuine, state (a patch
whose predecessor is missing simply goes void). There is no way to tell from
this directory alone; compare two exports, or obtain one from another member,
if that matters.

**Whose republic this is.** The check runs against the roster inside the
bundle. It proves that THAT roster approved these patches - anyone can found a
republic and export a bundle from it. Compare the republic id and the member
keys the verifier prints with the ones you expect.

Local, unapplied drafts are never exported: what you see is the approved fold.
"#;
