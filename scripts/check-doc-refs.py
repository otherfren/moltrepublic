#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check that every document reference in the tree resolves.

Code comments cite doc paths heavily, so moving a document silently rots them.
This walks every .rs/.md/.slint/.sh/.toml file and resolves two kinds of
reference:

  PATH  an explicit `docs/...md` or `docs_archive/...md` — must exist
  NAME  a bare `something.md` mentioned in prose — must match exactly one
        document in the tree (an ambiguous basename is reported too, because
        it means a reader cannot tell which file is meant)

Run it after moving or renaming any document:

    python3 scripts/check-doc-refs.py

Exit status is non-zero when something does not resolve.
"""

import pathlib
import re
import sys

SCAN_SUFFIXES = {".rs", ".md", ".slint", ".sh", ".toml"}
SKIP_PREFIXES = ("target/", ".git/", ".claude/")

# Names that look like document references but are not. Each is a real string
# in the tree with a reason it must be ignored — keep the reason, or the next
# person will "fix" it back into a false positive.
NOT_REFERENCES = {
    # test fixtures: files a transfer test actually shares over the wire
    "draft.md": "fixture in molt-engine tests",
    "notes.md": "fixture in molt-engine tests",
    "satzung.md": "fixture in tests/file_transfer.rs",
    # a working document of that session, absorbed into the doc that cites it
    "fixme.md": "historical prose in docs_archive/transport/mesh/stage_b.md",
}

# Paths that point INTO ANOTHER REPOSITORY (we cite their layout, not ours).
FOREIGN_PATHS = {
    "docs/marmot-architecture/distributed-convergence.md": "MDK repo",
    "docs/multi-tenant-conformance.md": "block/buzz repo",
    "docs/git-on-object-storage.md": "block/buzz repo",
}

# `.slint` carries mock document names for the UI mockups, never references.
NO_BARE_SCAN = {".slint"}

# Rust files whose bare `*.md` strings are the Multisig-Wiki mock's SAMPLE
# DOCUMENTS (the same names the .slint mockups carried before the state
# machine moved into Rust, 2026-08-12) — wiki paths, never repo docs.
NO_BARE_SCAN_FILES = {
    "crates/molt-ui/src/wiki.rs": "wiki mock sample tree + its tests",
    "crates/molt-ui/src/patchview.rs": "diff-viewer tests over the sample tree",
    "crates/molt-ui/src/tests/gui/wiki.rs": "the headless wiki tests drive the sample tree by name",
    # the strict fold's fixtures + doc examples are WIKI paths (a.md, b.md,
    # folder/file.md), never repo docs — same class as the sample tree
    "crates/molt-core/src/wiki_fold.rs": "fold keystone fixtures",
    "crates/molt-core/src/lib.rs": "WikiDoc doc-comment path examples",
    "crates/molt-engine/src/chain.rs": "supersede-walk keystone fixtures",
    "crates/molt-engine/tests/wiki_export.rs": "wiki export keystone fixtures",
}

PATH_RE = re.compile(r"(?<![\w/-])((?:docs|docs_archive)/[A-Za-z0-9_./-]+\.md)")
# A bare name starts at a true boundary — a leading `-` would split
# `concept-config-bidirection.md` into a phantom `config-bidirection.md`.
BARE_RE = re.compile(r"(?<![\w/.-])([a-z][a-z0-9_]*(?:-[a-z0-9_]+)*\.md)")


def keep(path: pathlib.Path) -> bool:
    s = str(path)
    return not s.startswith(SKIP_PREFIXES) and "/target/" not in s


def main() -> int:
    root = pathlib.Path(".")
    index: dict[str, list[str]] = {}
    for p in root.rglob("*.md"):
        if keep(p):
            index.setdefault(p.name, []).append(str(p))

    files = sorted({p for p in root.rglob("*") if p.suffix in SCAN_SUFFIXES and p.is_file() and keep(p)})
    broken: list[tuple[str, str, str]] = []
    ambiguous: list[tuple[str, str, list[str]]] = []
    resolved = 0

    for f in files:
        try:
            text = f.read_text()
        except (OSError, UnicodeDecodeError):
            continue
        for m in PATH_RE.finditer(text):
            target = m.group(1)
            if target in FOREIGN_PATHS or pathlib.Path(target).exists():
                resolved += 1
            else:
                broken.append((str(f), target, "PATH"))
        if f.suffix in NO_BARE_SCAN or str(f) in NO_BARE_SCAN_FILES:
            continue
        for m in BARE_RE.finditer(text):
            name = m.group(1)
            if name in NOT_REFERENCES:
                continue
            hits = index.get(name)
            if hits is None:
                broken.append((str(f), name, "NAME"))
            elif len(hits) > 1:
                ambiguous.append((str(f), name, hits))
            else:
                resolved += 1

    print(f"resolved: {resolved}")
    for f, t, kind in broken:
        print(f"BROKEN [{kind}] {f} -> {t}")
    for f, n, hits in ambiguous:
        print(f"AMBIGUOUS {f} -> {n} :: {hits}")
    if broken or ambiguous:
        print(f"\n{len(broken)} broken, {len(ambiguous)} ambiguous")
        return 1
    print("all document references resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
