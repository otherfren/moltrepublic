#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Developer test: a REAL 2-of-3 republic over a live SMP server.

Boots THREE headless moltd instances (each with its own config.toml and
workspace dir), founds a 2-of-3 DAO across them over the public SMP server
(smp8.simplex.im — the config default), and then exercises every
Organization/Status function end to end, verifying the EFFECT on all three
nodes:

  * set_name            → status.name + read_session entry rename everywhere
  * set_charter         → status.agenda everywhere
  * set_image           → real PNG bytes ride the proposal; logo file
                          materializes in every node's workspace dir
  * set_chat_retention  → status.chat_retention_days everywhere
  * share_file          → real file + real sha256 on node A
  * download_file       → node B pulls the BYTES peer-to-peer; sha256 of the
                          landed file equals the anchored checksum
  * negative            → downloading A's share while A is STOPPED fails
                          honestly (timeout error, no file)

Usage:  python3 scripts/dev_2of3_smp.py [--keep] [--moltd PATH]

The script builds moltd if --moltd is not given, runs everything under a
scratch directory, prints a step-by-step protocol, and cleans up (unless
--keep). Exit code 0 = every check passed.
"""

import argparse
import hashlib
import json
import os
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SMP_NOTE = "public (smp8.simplex.im, the config default)"
PORTS = (4041, 4042, 4043)
NAMES = ("anna", "bela", "cora")
DAO_NAME = "Drei Instanzen"
DAO_AGENDA = "über echtes SMP regieren"

# a tiny but real PNG (1x1, red) — what set_image embeds
TINY_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108020000009077"
    "53de0000000c4944415408d763f8cfc000000301010018dd8db00000000049"
    "454e44ae426082"
)


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


class Node:
    """One headless moltd instance driven over MCP TCP."""

    def __init__(self, name: str, port: int, root: Path, moltd: Path):
        self.name = name
        self.port = port
        self.dir = root / name
        self.ws_dir = self.dir / "workspaces"
        self.dl_dir = self.dir / "downloads"
        self.token = secrets.token_hex(24)
        self.moltd = moltd
        self.proc: subprocess.Popen | None = None
        self.sock: socket.socket | None = None
        self.rpc_id = 0
        self.ws_dir.mkdir(parents=True, exist_ok=True)
        self.dl_dir.mkdir(parents=True, exist_ok=True)
        (self.dir / "config.toml").write_text(
            f"""[node]
headless = true

[storage]
workspace_dir = "{self.ws_dir}"
download_dir = "{self.dl_dir}"

[mcp]
port = {port}
allow = "127.0.0.1"
token = "{self.token}"

[transport.anonymity]
network = "none"

[transport.smp]
server = "public"

[ui]
lang = "de"
theme = "dark"
"""
        )

    def start(self) -> None:
        logf = open(self.dir / "moltd.log", "ab")
        # keep stdin OPEN: headless moltd also serves MCP over stdio and
        # exits once stdin reaches EOF — we drive it over TCP instead
        self.proc = subprocess.Popen(
            [str(self.moltd), "--config", str(self.dir / "config.toml")],
            stdin=subprocess.PIPE,
            stdout=logf,
            stderr=logf,
            cwd=self.dir,
        )
        # await the MCP TCP endpoint + authenticate
        deadline = time.time() + 30
        while True:
            try:
                self.sock = socket.create_connection(("127.0.0.1", self.port), timeout=5)
                break
            except OSError:
                if time.time() > deadline:
                    raise RuntimeError(f"{self.name}: MCP port never came up")
                time.sleep(0.3)
        self.sock_file = self.sock.makefile("rwb")
        init = self._rpc("initialize", {"token": self.token})
        assert "result" in init, f"{self.name}: initialize failed: {init}"
        log(f"{self.name}: moltd up, MCP authenticated on :{self.port}")

    def stop(self) -> None:
        if self.sock:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            self.proc = None

    def _rpc(self, method: str, params: dict) -> dict:
        self.rpc_id += 1
        req = {"jsonrpc": "2.0", "id": self.rpc_id, "method": method, "params": params}
        self.sock_file.write((json.dumps(req) + "\n").encode())
        self.sock_file.flush()
        line = self.sock_file.readline()
        if not line:
            raise RuntimeError(f"{self.name}: MCP connection closed")
        return json.loads(line)

    def tool(self, name: str, arguments: dict | None = None):
        """Call one MCP tool; returns the parsed Reply JSON."""
        resp = self._rpc("tools/call", {"name": name, "arguments": arguments or {}})
        result = resp.get("result", {})
        text = result.get("content", [{}])[0].get("text", "")
        if result.get("isError"):
            raise ToolError(text)
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return text

    def poll(self, what: str, fn, timeout: float = 60.0, every: float = 0.5):
        """Poll `fn` (returns a truthy value when satisfied)."""
        deadline = time.time() + timeout
        while True:
            value = fn()
            if value:
                return value
            if time.time() > deadline:
                raise RuntimeError(f"{self.name}: timed out waiting for {what}")
            time.sleep(every)


class ToolError(RuntimeError):
    pass


def found_2_of_3(a: Node, b: Node, c: Node) -> None:
    log(f"founding '{DAO_NAME}' as 2-of-3 over {SMP_NOTE} …")
    a.tool("create_start", {
        "name": DAO_NAME, "member": a.name, "threshold": 2, "members": 3,
    })

    def has_handover(link: str) -> bool:
        # a joinable link's LAST segment is the hex-encoded transport
        # handover (server/queue/wrap/seat) — a short pre-provisioning
        # link ends in the ticket only
        blob = link.rsplit("/", 1)[-1]
        return (
            link.startswith("molt://")
            and len(blob) >= 64
            and all(ch in "0123456789abcdef" for ch in blob)
        )

    def seat_links():
        s = a.tool("read_session")
        seats = s.get("create", {}).get("seats", [])
        links = [seat.get("link", "") for seat in seats]
        if len(links) == 2 and all(has_handover(l) for l in links):
            return links
        return None

    links = a.poll("both seat links (SMP queue provisioning)", seat_links, timeout=60)
    log("founder minted 2 invite links (real SMP queues)")

    b.tool("join_start", {"invite": links[0], "member": b.name})
    c.tool("join_start", {"invite": links[1], "member": c.name})

    def joined():
        create = a.tool("read_session")["create"]
        if create.get("can_propose"):
            return True
        states = [(seat.get("member") or "?", seat.get("state")) for seat in create.get("seats", [])]
        joiner_logs = {
            n.name: n.tool("read_session")["join"].get("log", [])[-1:]
            for n in (b, c)
        }
        log(f"  … seats: {states} · joins: {joiner_logs}")
        return False

    a.poll("both members joined (can_propose)", joined, timeout=240, every=5)
    log("both members joined — founder proposes the charter")
    a.tool("create_propose", {"name": DAO_NAME, "agenda": DAO_AGENDA})

    for joiner in (b, c):
        joiner.poll(
            "the charter to ratify",
            lambda j=joiner: j.tool("read_session")["join"].get("awaiting_ratify"),
            timeout=90,
        )
        joiner.tool("join_confirm_charter")
        log(f"{joiner.name}: charter ratified")

    a.poll(
        "the founder's seal",
        lambda: a.tool("read_session")["create"]["outcome"] == 1,
        timeout=120,
    )
    a.tool("create_finish")
    for n in (a, b, c):
        n.poll(
            "the workspace to open",
            lambda n=n: n.tool("read_session").get("active_workspace"),
            timeout=90,
        )
    log("SEALED: 2-of-3 republic open on all three nodes ✓")


def approve_latest(node: Node, surface: str = "organization") -> None:
    """Approve the newest pending proposal on `node` (the second signature)."""
    def pending_id():
        snap = node.tool("read_state", {"surface": surface})
        pend = snap.get("pending", [])
        # wait until the proposal gossip reached this node
        mine = [p for p in pend if not p.get("approved_by_me")]
        return mine[-1]["id"] if mine else None

    pid = node.poll("the proposal gossip", pending_id, timeout=60)
    node.tool("approve", {"proposal_id": pid})


def assert_everywhere(nodes, what: str, fn, timeout: float = 60.0) -> None:
    for n in nodes:
        n.poll(f"{what}", lambda n=n: fn(n), timeout=timeout)
    log(f"{what} ✓ (alle 3 Knoten)")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="keep the scratch dir + logs")
    ap.add_argument("--moltd", type=Path, help="use this moltd binary (skip cargo build)")
    args = ap.parse_args()

    moltd = args.moltd
    if not moltd:
        log("building moltd (cargo build -p molt-app) …")
        subprocess.run(
            ["cargo", "build", "-p", "molt-app"], cwd=REPO, check=True
        )
        moltd = REPO / "target" / "debug" / "moltd"
    moltd = moltd.resolve()

    scratch = Path(tempfile.mkdtemp(prefix="molt-2of3-"))
    log(f"scratch: {scratch}")
    nodes = [Node(n, p, scratch, moltd) for n, p in zip(NAMES, PORTS)]
    a, b, c = nodes
    failures: list[str] = []

    try:
        for n in nodes:
            n.start()

        found_2_of_3(a, b, c)

        # ---------------- set_name ----------------
        a.tool("propose", {"surface": "organization", "payload": {
            "op": "set_name", "title": "Namen ändern", "value": "Umbenannt e.V.",
        }})
        approve_latest(b)
        assert_everywhere(
            nodes,
            "set_name wirksam (status.name + Session-Eintrag)",
            lambda n: n.tool("status").get("name") == "Umbenannt e.V."
            and any(
                w.get("name") == "Umbenannt e.V."
                for w in n.tool("read_session").get("workspaces", [])
            ),
        )

        # ---------------- set_charter ----------------
        a.tool("propose", {"surface": "organization", "payload": {
            "op": "set_charter", "title": "Satzung ändern",
            "value": "wir regieren jetzt mit neuer satzung",
        }})
        approve_latest(c)  # this time C provides the second signature
        assert_everywhere(
            nodes,
            "set_charter wirksam (status.agenda)",
            lambda n: n.tool("status").get("agenda")
            == "wir regieren jetzt mit neuer satzung",
        )

        # ---------------- set_image ----------------
        import base64
        a.tool("propose", {"surface": "organization", "payload": {
            "op": "set_image", "title": "Logo setzen", "value": "logo.png",
            "bytes_b64": base64.b64encode(TINY_PNG).decode(),
        }})
        approve_latest(b)

        def logo_ok(n: Node):
            image = n.tool("status").get("image", "")
            if not image.endswith("logo.png"):
                return False
            p = Path(image)
            return p.is_file() and p.read_bytes() == TINY_PNG

        assert_everywhere(
            nodes, "set_image wirksam (logo.png byte-identisch materialisiert)", logo_ok
        )

        # ---------------- set_chat_retention ----------------
        a.tool("propose", {"surface": "organization", "payload": {
            "op": "set_chat_retention", "title": "Löschfrist ändern", "value": "30 days",
        }})
        approve_latest(b)
        assert_everywhere(
            nodes,
            "set_chat_retention wirksam (status.chat_retention_days == 30)",
            lambda n: n.tool("status").get("chat_retention_days") == 30,
        )

        # ---------------- share_file + p2p download ----------------
        share_src = scratch / "unterlagen.bin"
        payload = secrets.token_bytes(700 * 1024)  # 3 pieces
        share_src.write_bytes(payload)
        want_sha = hashlib.sha256(payload).hexdigest()
        a.tool("share_file", {"path": str(share_src)})

        def share_row(n: Node):
            for u in n.tool("read_uploads").get("uploads", []):
                if u.get("name") == "unterlagen.bin":
                    return u
            return None

        row = a.poll("the share to post (async hash)", lambda: share_row(a), timeout=60)
        assert row["checksum"] == want_sha, "anchored checksum mismatch on A"
        assert_everywhere(
            nodes[1:],
            "share bei B und C angekommen (echte sha256 im Log)",
            lambda n: (share_row(n) or {}).get("checksum") == want_sha,
        )

        share_id = share_row(b)["id"]
        b.tool("download_file", {"id": share_id, "dest": str(b.dl_dir)})

        def downloaded():
            u = share_row(b)
            d = (u or {}).get("download")
            if d and d.get("phase") == "failed":
                raise RuntimeError(f"download failed: {d.get('error')}")
            return d and d.get("phase") == "done" and d.get("path")

        path = b.poll("den P2P-Download (A → B)", downloaded, timeout=180)
        got = Path(path).read_bytes()
        assert hashlib.sha256(got).hexdigest() == want_sha, "downloaded bytes differ!"
        log(f"P2P-Download ✓ ({len(got)} bytes, sha256 ok → {path})")

        # ---------------- negative: sharer offline ----------------
        log("negative Probe: A stoppen, C lädt — muss ehrlich scheitern …")
        a.stop()
        c.tool("download_file", {"id": share_id, "dest": str(c.dl_dir)})

        def failed_honestly():
            u = share_row(c)
            d = (u or {}).get("download")
            return d and d.get("phase") == "failed" and d.get("error")

        err = c.poll("den ehrlichen Offline-Fehler", failed_honestly, timeout=180)
        log(f"Offline-Download scheitert ehrlich ✓ ({err!r})")
        assert not list(c.dl_dir.iterdir()), "no file may land on C"

    except (Exception,) as e:  # noqa: BLE001 — the protocol prints the failure
        failures.append(str(e))
        log(f"FEHLGESCHLAGEN: {e}")
    finally:
        for n in nodes:
            n.stop()
        if args.keep or failures:
            log(f"scratch behalten: {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)

    if failures:
        return 1
    log("ALLE PRÜFUNGEN BESTANDEN ✓ (2-of-3 über echtes SMP)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
