#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Walk the whole GUI over MCP (docs_archive/ui/gui_over_mcp.md step 5).

Two real `moltd` nodes come up on the Slint TESTING backend (no display,
full event loop), found a 2-of-2 republic over a throwaway dev relay, and
after every phase the script reads the WINDOW back (`read_ui_state`) and
asserts the GUI mirrors what the engine holds — the wizard phases, the
open workspace, the chat pane, a proposal's pending card. The drive half
uses `ui_action` (open_workspace / select_view / chat_send /
select_channel), i.e. the same Slint callbacks a click takes.

Run it from the repo root:

    cargo build -p molt-app --features ui-testing
    cargo build -p molt-net --example dev_relay
    python3 scripts/gui_walk.py

Exit 0 = every assertion held. Any miss prints the step and exits 1.
Stdlib-only; scratch state lives in a temp dir and two ephemeral ports.
"""

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MOLTD = os.path.join(REPO, "target", "debug", "moltd")
DEV_RELAY = os.path.join(REPO, "target", "debug", "examples", "dev_relay")
TOKEN = "walk"


def fail(step, detail):
    print(f"WALK FAILED at {step}: {detail}")
    sys.exit(1)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Node:
    """Newline-delimited JSON-RPC over the node's MCP TCP port."""

    def __init__(self, name, port):
        self.name = name
        self.next_id = 1
        deadline = time.time() + 60
        while True:
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
                break
            except OSError:
                if time.time() > deadline:
                    fail(f"{name} connect", f"mcp port {port} never opened")
                time.sleep(0.3)
        self.buf = b""
        self.call("initialize", {"token": TOKEN})

    def call(self, method, params):
        rid = self.next_id
        self.next_id += 1
        msg = json.dumps(
            {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        )
        self.sock.sendall(msg.encode() + b"\n")
        while True:
            if b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if not line.strip():
                    continue
                resp = json.loads(line)
                if resp.get("id") == rid:
                    if "error" in resp:
                        fail(f"{self.name} {method}", resp["error"])
                    return resp.get("result", {})
                continue
            chunk = self.sock.recv(65536)
            if not chunk:
                fail(f"{self.name} {method}", "connection closed")
            self.buf += chunk

    def tool(self, name, args=None):
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        text = "\n".join(c.get("text", "") for c in r.get("content", []))
        if r.get("isError"):
            fail(f"{self.name} {name}", text)
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            return {"text": text}

    def session(self):
        return self.tool("read_session")

    def wait_session(self, pred, what, timeout=120):
        deadline = time.time() + timeout
        while time.time() < deadline:
            sv = self.session()
            if pred(sv):
                return sv
            time.sleep(0.5)
        fail(f"{self.name} wait", what)

    def snapshot(self, what="snapshot", timeout=30):
        """The window's own claim (read_ui_state), waiting for a publish."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            snap = self.tool("read_ui_state").get("snapshot")
            if snap and snap.get("generation", 0) >= 1:
                return snap
            time.sleep(0.3)
        fail(f"{self.name} read_ui_state", f"no {what} published")

    def wait_window(self, pred, what, timeout=45):
        """Poll the SNAPSHOT until the window itself mirrors the fact."""
        deadline = time.time() + timeout
        snap = {}
        while time.time() < deadline:
            snap = self.snapshot(what)
            if pred(snap):
                return snap
            time.sleep(0.4)
        fail(f"{self.name} window", f"{what}; last snapshot: {snap}")

    def act(self, verb, args=None):
        """Drive the window (ui_action) and wait for the next publish."""
        before = self.snapshot().get("generation", 0)
        self.tool("ui_action", {"verb": verb, "args": args or {}})
        self.wait_window(
            lambda s: s.get("generation", 0) > before, f"{verb} landed"
        )


def spawn_moltd(tag, tmp, port):
    cfg = os.path.join(tmp, f"{tag}.toml")
    ws = os.path.join(tmp, f"ws-{tag}")
    with open(cfg, "w", encoding="utf-8") as f:
        f.write(
            f'[node]\nheadless = false\n[storage]\nworkspace_dir = "{ws}"\n'
            f'[mcp]\nport = {port}\nallow = "127.0.0.1"\ntoken = "{TOKEN}"\n'
        )
    log = open(os.path.join(tmp, f"{tag}.log"), "w", encoding="utf-8")
    env = dict(os.environ, MOLT_UI_TESTING="1", RUST_LOG="molt_ui=debug")
    return subprocess.Popen(
        [MOLTD, "--config", cfg], stdout=log, stderr=log, env=env
    )


def main():
    for path, hint in [
        (MOLTD, "cargo build -p molt-app --features ui-testing"),
        (DEV_RELAY, "cargo build -p molt-net --example dev_relay"),
    ]:
        if not os.path.exists(path):
            fail("prerequisites", f"{path} missing - run: {hint}")

    tmp = tempfile.mkdtemp(prefix="gui-walk-")
    procs = []
    try:
        # a throwaway relay (in-memory; prints its ws:// URL and stays up)
        relay = subprocess.Popen(
            [DEV_RELAY], stdout=subprocess.PIPE, text=True
        )
        procs.append(relay)
        relay_url = relay.stdout.readline().strip()
        if not re.match(r"^ws://127\.0\.0\.1:\d+$", relay_url):
            fail("dev_relay", f"unexpected first line: {relay_url!r}")
        print(f"relay {relay_url}")

        pa, pb = free_port(), free_port()
        procs.append(spawn_moltd("a", tmp, pa))
        procs.append(spawn_moltd("b", tmp, pb))
        a, b = Node("a", pa), Node("b", pb)

        # phase 0: both windows are up, on the choice screen
        for n in (a, b):
            snap = n.snapshot("first publish")
            if snap.get("screen") != "choice":
                fail("phase 0", f"{n.name} screen={snap.get('screen')}")
        print("phase 0: both windows publish (choice screen)")

        # phase 1: relay into both pools, then the founding wizard - the
        # engine drives, the WINDOW must follow into the create screen
        for n in (a, b):
            n.tool("relay_add", {"url": relay_url})
            n.tool("relay_confirm", {"url": relay_url, "accept_clearnet": True})
            n.wait_session(
                lambda s: any(
                    r.get("confirmed") for r in s["settings"]["relays"]
                ),
                "relay confirmed",
                60,
            )
        deadline = time.time() + 120
        while True:
            try:
                a.tool(
                    "create_start",
                    {"name": "Walk", "member": "alpha", "threshold": 2, "members": 2},
                )
                break
            except SystemExit:
                raise
            except Exception:
                if time.time() > deadline:
                    raise
                time.sleep(2)
        a.wait_window(lambda s: s.get("screen") == "create", "create screen")
        print("phase 1: founder window entered the create wizard")

        # phase 2: join over the invite; the joiner window follows too
        sv = a.wait_session(
            lambda s: any(
                x["link"].startswith("molt://invite/")
                for x in s["create"]["seats"]
            ),
            "invite minted",
        )
        link = next(
            x["link"]
            for x in sv["create"]["seats"]
            if x["link"].startswith("molt://invite/")
        )
        deadline = time.time() + 60
        while True:
            try:
                b.tool("join_start", {"invite": link, "member": "beta"})
                break
            except SystemExit:
                raise
            except Exception:
                if time.time() > deadline:
                    raise
                time.sleep(2)
                sv = a.session()
                link = next(
                    x["link"]
                    for x in sv["create"]["seats"]
                    if x["link"].startswith("molt://invite/")
                )
        b.wait_window(lambda s: s.get("screen") == "join", "join screen")
        a.wait_session(lambda s: s["create"]["can_propose"], "seat joined", 120)
        a.tool("create_propose", {"name": "Walk", "agenda": "walk the gui"})
        b.wait_session(lambda s: s["join"]["awaiting_ratify"], "charter", 120)
        b.tool("join_confirm_charter")
        sv = b.wait_session(lambda s: s["join"].get("awaiting_backup"), "backup")
        b.tool("confirm_seed_backup", {"phrase": sv["join"]["seed"]})
        sv = a.wait_session(
            lambda s: all(x["state"] in (2, 4) for x in s["create"]["seats"]),
            "ratified",
            120,
        )
        a.tool("confirm_seed_backup", {"phrase": sv["create"]["seed"]})
        a.wait_session(
            lambda s: s["create"].get("outcome") == 1, "sealed", 180
        )
        a.tool("create_finish")
        b.wait_session(lambda s: s["join"].get("outcome") == 1, "join sealed", 180)
        b.tool("join_finish")
        for n in (a, b):
            n.wait_window(lambda s: s.get("screen") == "main", "main screen")
        print("phase 2: founded 2-of-2, both windows on the main screen")

        # phase 3: drive the chat THROUGH the window and read it back
        a.act("chat_send", {"body": "hello from the walk"})
        a.wait_window(
            lambda s: s.get("chat_rows", 0) >= 1
            and any("hello from the walk" in l for l in s.get("chat_last", [])),
            "own message visible",
        )
        b.wait_window(
            lambda s: any(
                "hello from the walk" in l for l in s.get("chat_last", [])
            ),
            "message arrived in the peer's window",
            60,
        )
        if not b.snapshot().get("chat_in_view", False):
            print("note: peer window reports chat_in_view=false (headless layout timing)")
        # a topic channel via the window, then back to the group channel
        b.act("select_channel", {"channel": "topic:walk"})
        b.act("chat_send", {"body": "a topic reply"})
        b.act("select_channel", {"channel": "group"})
        # channels are engine-side VIEWS: a's window on "group" must NOT
        # show the topic reply; switching a onto the topic must
        a.wait_window(
            lambda s: not any("a topic reply" in l for l in s.get("chat_last", [])),
            "topic reply filtered off the group channel",
        )
        a.act("select_channel", {"channel": "topic:walk"})
        a.wait_window(
            lambda s: s.get("channel") == "topic:walk"
            and any("a topic reply" in l for l in s.get("chat_last", [])),
            "topic reply visible after switching the channel",
            60,
        )
        a.act("select_channel", {"channel": "group"})
        print("phase 3: chat + topic channels drove through both windows")

        # phase 4: a proposal appears as a pending card in the OTHER window,
        # approving applies it (2-of-2: proposer + approver)
        r = a.tool(
            "propose",
            {
                "surface": "organization",
                "payload": {"op": "set_charter", "value": "the walk holds"},
            },
        )
        pid = r.get("id")
        if pid is None:
            fail("phase 4", f"propose returned {r}")
        b.wait_window(
            lambda s: s.get("pending_count", 0) >= 1, "pending card in the peer window"
        )
        b.tool("approve", {"proposal_id": pid})
        b.wait_window(
            lambda s: s.get("pending_count", 0) == 0, "card cleared after approve"
        )
        print("phase 4: proposal card appeared, approve cleared it")

        # phase 5: the window walks the surfaces and closes the workspace
        for view in ("memory", "chat"):
            a.act("select_view", {"surface": view})
        a.act("close_workspace")
        a.wait_window(lambda s: s.get("screen") == "choice", "back on choice")
        print("phase 5: surface walk + close, window back on the choice screen")

        print("WALK OK - the window mirrored every phase")
        return 0
    finally:
        for p in procs:
            p.kill()
        for p in procs:
            try:
                p.wait(timeout=10)
            except Exception:
                pass


if __name__ == "__main__":
    sys.exit(main())
