#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""A 2-of-4 vault republic on one machine (docs/vault/vault_build_plan.md U3, section 5).

Headless seats s1..sN run the live-preview `moltd` built with the
`vault-lab` feature and are driven over MCP; the GUI is the fourth seat
(or, with --founder-headless, a headless founder `f`).

    CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 \\
        cargo build -j 2 -p molt-app --features live-preview,vault-lab
    cargo run -p molt-net --example dev_relay        # prints ws://127.0.0.1:<port>
    python3 scripts/vault_lab.py up --relay ws://127.0.0.1:<port> [--seats 3] [--complainer s3] [--founder-headless]
    python3 scripts/vault_lab.py join
    python3 scripts/vault_lab.py seal s1 a one
    python3 scripts/vault_lab.py approve
    python3 scripts/vault_lab.py grant a s2
    python3 scripts/vault_lab.py approve
    python3 scripts/vault_lab.py read s2 a
    python3 scripts/vault_lab.py stop|start <seat> | status | down

Stdlib only. State lives in /tmp/vault-lab (lab.json, one dir per seat).
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MOLTD = os.path.join(REPO, "target", "dev-ui", "debug", "moltd")
LAB = "/tmp/vault-lab"
STATE = os.path.join(LAB, "lab.json")
TOKEN = "vault-lab"
FOUNDER = "f"
REPUBLIC = "VaultLab"


class LabError(Exception):
    pass


def die(msg):
    print(f"error: {msg}")
    sys.exit(1)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def load():
    try:
        with open(STATE, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        die("no lab - run: vault_lab.py up --relay <url>")


def save(state):
    tmp = STATE + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, indent=2)
    os.replace(tmp, STATE)


def alive(pid):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    # a zombie child of an exited `up` is reaped by init; treat it as gone
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8") as f:
            return f.read().split(")")[-1].split()[0] != "Z"
    except OSError:
        return False


class Node:
    """Newline-delimited JSON-RPC over a seat's MCP TCP port."""

    def __init__(self, name, port, timeout=60):
        self.name = name
        self.next_id = 1
        deadline = time.time() + timeout
        while True:
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=120)
                break
            except OSError:
                if time.time() > deadline:
                    raise LabError(f"{name}: mcp port {port} never opened")
                time.sleep(0.3)
        self.buf = b""
        self.call("initialize", {"token": TOKEN})

    def call(self, method, params):
        rid = self.next_id
        self.next_id += 1
        msg = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        self.sock.sendall(msg.encode() + b"\n")
        while True:
            if b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if not line.strip():
                    continue
                resp = json.loads(line)
                if resp.get("id") != rid:
                    continue
                if "error" in resp:
                    raise LabError(f"{self.name} {method}: {resp['error']}")
                return resp.get("result", {})
            chunk = self.sock.recv(65536)
            if not chunk:
                raise LabError(f"{self.name} {method}: connection closed")
            self.buf += chunk

    def try_tool(self, name, args=None):
        """`(ok, value)`: the parsed reply, or the refusal text."""
        r = self.call("tools/call", {"name": name, "arguments": args or {}})
        text = "\n".join(c.get("text", "") for c in r.get("content", []))
        if r.get("isError"):
            return False, text
        try:
            return True, json.loads(text)
        except json.JSONDecodeError:
            return True, {"text": text}

    def tool(self, name, args=None):
        ok, v = self.try_tool(name, args)
        if not ok:
            raise LabError(f"{self.name} {name}: {v}")
        return v

    def session(self):
        return self.tool("read_session")

    def wait_session(self, pred, what, timeout=180):
        deadline = time.time() + timeout
        while time.time() < deadline:
            sv = self.session()
            if pred(sv):
                return sv
            time.sleep(0.5)
        raise LabError(f"{self.name}: timed out waiting for {what}")

    def vault(self):
        ok, v = self.try_tool("read_state", {"surface": "vault"})
        return (v.get("vault") or {}) if ok else {}

    def pending(self, surface):
        ok, v = self.try_tool("read_state", {"surface": surface})
        if not ok:
            return []
        return [p for p in v.get("pending", []) if p.get("state") == "proposed"]


def node(state, seat, timeout=60):
    s = state["seats"].get(seat)
    if s is None:
        die(f"unknown seat {seat} (seats: {', '.join(sorted(state['seats']))})")
    return Node(seat, s["port"], timeout)


def headless(state):
    """The headless seats in name order, founder first."""
    return sorted(state["seats"], key=lambda n: (n != FOUNDER, n))


def config_text(ws_dir, port, relay, headless_seat, open_on_start=""):
    node_lines = f"headless = {'true' if headless_seat else 'false'}\n"
    if open_on_start:
        node_lines += f'open_on_start = "{open_on_start}"\n'
    return (
        f"[node]\n{node_lines}"
        f'[storage]\nworkspace_dir = "{ws_dir}"\n'
        f'[mcp]\nport = {port}\nallow = "127.0.0.1"\ntoken = "{TOKEN}"\n'
        f"[transport.nostr]\nclearnet_enabled = true\n"
        f'[[transport.nostr.relay]]\nurl = "{relay}"\nconfirmed = true\n'
    )


def write_config(state, seat):
    s = state["seats"][seat]
    with open(os.path.join(s["dir"], "config.toml"), "w", encoding="utf-8") as f:
        f.write(
            config_text(
                os.path.join(s["dir"], "ws"), s["port"], state["relay"], True, s.get("workspace", "")
            )
        )


def spawn(state, seat):
    s = state["seats"][seat]
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "info")
    env.pop(LAB_ENV, None)
    if seat == state.get("complainer"):
        env[LAB_ENV] = "1"
    log = open(os.path.join(s["dir"], "moltd.log"), "a", encoding="utf-8")
    p = subprocess.Popen(
        [MOLTD, "--config", os.path.join(s["dir"], "config.toml"), "--mcp-tcp", f"127.0.0.1:{s['tcp']}"],
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=log,
        env=env,
        start_new_session=True,
    )
    s["pid"] = p.pid


LAB_ENV = "MOLT_VAULT_LAB_COMPLAIN"


def binary_is_fresh():
    if not os.path.exists(MOLTD):
        return False, "missing"
    built = os.path.getmtime(MOLTD)
    for root, dirs, files in os.walk(os.path.join(REPO, "crates")):
        dirs[:] = [d for d in dirs if d != "target"]
        for f in files:
            if f.endswith(".rs") and os.path.getmtime(os.path.join(root, f)) > built:
                return False, f"older than {os.path.relpath(os.path.join(root, f), REPO)}"
    return True, ""


def wait_relay(n):
    n.wait_session(
        lambda sv: any(r.get("confirmed") for r in sv["settings"]["relays"]),
        "relay confirmed",
        120,
    )


def cmd_up(a):
    fresh, why = binary_is_fresh()
    if not fresh:
        die(
            f"{MOLTD} {why} - build: CARGO_TARGET_DIR=target/dev-ui SLINT_LIVE_PREVIEW=1 "
            "cargo build -j 2 -p molt-app --features live-preview,vault-lab"
        )
    if os.path.exists(STATE):
        old = load()
        if any(alive(s.get("pid")) for s in old["seats"].values()):
            die("a lab is up - run: vault_lab.py down")
        shutil.rmtree(LAB, ignore_errors=True)
    names = [f"s{i}" for i in range(1, a.seats + 1)]
    if a.complainer and a.complainer not in names:
        die(f"--complainer {a.complainer} is not a seat")
    if a.founder_headless:
        names.insert(0, FOUNDER)
    os.makedirs(LAB, exist_ok=True)
    state = {
        "relay": a.relay,
        "complainer": a.complainer or "",
        "founder_headless": a.founder_headless,
        "members": a.seats + 1,
        "threshold": a.threshold,
        "seats": {},
    }
    for name in names:
        d = os.path.join(LAB, name)
        os.makedirs(d, exist_ok=True)
        state["seats"][name] = {"dir": d, "port": free_port(), "tcp": free_port(), "pid": 0, "workspace": ""}
        write_config(state, name)
        spawn(state, name)
    if not a.founder_headless:
        gui = os.path.join(LAB, "gui")
        os.makedirs(gui, exist_ok=True)
        port = free_port()
        state["gui_port"] = port
        with open(os.path.join(gui, "config.toml"), "w", encoding="utf-8") as f:
            f.write(config_text(os.path.join(gui, "ws"), port, a.relay, False))
    save(state)
    for name in names:
        wait_relay(node(state, name))
        print(f"{name} up port={state['seats'][name]['port']}")
    if a.founder_headless:
        f = node(state, FOUNDER)
        deadline = time.time() + 120
        while True:
            ok, v = f.try_tool(
                "create_start",
                {"name": REPUBLIC, "member": FOUNDER, "threshold": a.threshold, "members": a.seats + 1},
            )
            if ok:
                break
            if time.time() > deadline:
                die(f"create_start: {v}")
            time.sleep(2)
        print(f"founder {FOUNDER}: create_start {a.threshold}-of-{a.seats + 1}")
    else:
        print("gui: SLINT_LIVE_PREVIEW=1 target/dev-ui/debug/moltd --config /tmp/vault-lab/gui/config.toml")


def founder_node(state):
    if state["founder_headless"]:
        return node(state, FOUNDER)
    port = state.get("gui_port")
    try:
        return Node("gui", port, timeout=5)
    except LabError:
        die("the GUI seat is not running - start it with the printed command")


def invites(sv):
    return [
        x["link"]
        for x in sv["create"]["seats"]
        if x["link"].startswith("molt://invite/") and not x["member"]
    ]


def cmd_join(_a):
    state = load()
    f = founder_node(state)
    joiners = [n for n in headless(state) if n != FOUNDER]
    nodes = {n: node(state, n) for n in joiners}
    # a seat's row names its member only once the activation arrives, so
    # every link this run handed out counts as taken
    used = set()
    for name in joiners:
        deadline = time.time() + 180
        while True:
            sv = f.session()
            links = [l for l in invites(sv) if l not in used]
            ok, v = (False, "no invite yet")
            for link in links:
                ok, v = nodes[name].try_tool("join_start", {"invite": link, "member": name})
                if ok:
                    used.add(link)
                    break
            if ok:
                break
            if time.time() > deadline:
                die(f"{name} join_start: {v}")
            time.sleep(2)
        print(f"{name} joining")
    f.wait_session(lambda s: s["create"]["can_propose"], "every seat joined", 300)
    if state["founder_headless"]:
        f.tool("create_propose", {"name": REPUBLIC, "agenda": "keep secrets", "features": ["vault"]})
        print(f"{FOUNDER}: charter proposed with the vault")
    else:
        print("waiting for the charter - tick Vault and propose it in the GUI")
    for name in joiners:
        n = nodes[name]
        sv = n.wait_session(lambda s: s["join"]["awaiting_ratify"], "the charter", 900)
        feats = sv["join"].get("proposed_features") or []
        n.tool("join_confirm_charter")
        sv = n.wait_session(lambda s: s["join"].get("awaiting_backup"), "the backup step")
        n.tool("confirm_seed_backup", {"phrase": sv["join"]["seed"]})
        print(f"{name} ratified features={','.join(feats) or '-'}")
    if state["founder_headless"]:
        sv = f.wait_session(
            lambda s: all(x["state"] in (2, 4) for x in s["create"]["seats"]), "ratified", 300
        )
        f.tool("confirm_seed_backup", {"phrase": sv["create"]["seed"]})
        f.wait_session(lambda s: s["create"].get("outcome") == 1, "sealed", 300)
        f.tool("create_finish")
        sv = f.wait_session(lambda s: s.get("active_workspace"), "founder workspace open", 60)
        state["seats"][FOUNDER]["workspace"] = sv["active_workspace"]
    else:
        print("waiting for the seal - confirm your seed backup in the GUI")
    for name in joiners:
        n = nodes[name]
        n.wait_session(lambda s: s["join"].get("outcome") == 1, "the seal", 900)
        n.tool("join_finish")
        sv = n.wait_session(lambda s: s.get("active_workspace"), "workspace open", 60)
        state["seats"][name]["workspace"] = sv["active_workspace"]
    for name in headless(state):
        write_config(state, name)
    save(state)
    print("founded")


def secret_of(n, name, reader=None, depositor=None):
    """The current committed version named `name` as `n` sees it."""
    v = n.vault()
    deps = [d for d in v.get("deposits", []) if d.get("name") == name]
    if depositor:
        deps = [d for d in deps if d.get("depositor") == depositor]
    if not deps:
        raise LabError(f"{n.name}: no deposit named {name}")
    owners = sorted({d.get("depositor") for d in deps})
    if len(owners) > 1:
        raise LabError(f"ambiguous name={name} depositors={','.join(owners)}")
    if reader:
        granted = {
            g["secret_id"]
            for g in v.get("grants", [])
            if g.get("reader") == reader and g.get("state") == "committed"
        }
        mine = [d for d in deps if d["secret_id"] in granted]
        if mine:
            return mine[-1]["secret_id"]
    live = [d for d in deps if d.get("state") != "pending"]
    return (live or deps)[-1]["secret_id"]


def cmd_seal(a):
    state = load()
    r = node(state, a.seat).tool("vault_seal", {"name": a.name, "kind": a.kind, "text": a.text})
    print(f"{a.seat} sealed {a.name} proposal={r.get('id', r)}")


def cmd_grant(a):
    state = load()
    by = a.by or headless(state)[0]
    n = node(state, by)
    sid = secret_of(n, a.name, depositor=a.depositor)
    r = n.tool("vault_grant", {"secret_id": sid, "reader": a.reader})
    print(f"{by} proposed grant {a.name} to {a.reader} proposal={r.get('id', r)}")


RETRY = ("payload not held", "not verified")
# a payload fetch takes tens of seconds over a relay; ten tries span it
RETRY_GAP = 10


def cmd_approve(a):
    state = load()
    seats = [s for s in headless(state) if alive(state["seats"][s]["pid"])]
    nodes = {s: node(state, s) for s in seats}
    tries = {}
    next_try = {}
    refused = set()
    approved = 0
    quiet_since = time.time()
    deadline = time.time() + a.timeout
    open_ids = set()
    while time.time() < deadline:
        busy = False
        open_ids = set()
        for s, n in nodes.items():
            for surface in ("vault", "organization"):
                for p in n.pending(surface):
                    busy = True
                    open_ids.add(p["id"])
                    key = (s, p["id"])
                    if p.get("approved_by_me") or p.get("declined_by_me") or key in refused:
                        continue
                    if time.time() < next_try.get(key, 0):
                        continue
                    ok, why = n.try_tool("approve", {"proposal_id": p["id"]})
                    if ok:
                        approved += 1
                        print(f"{s} approved {p['id']}")
                        continue
                    tries[key] = tries.get(key, 0) + 1
                    next_try[key] = time.time() + RETRY_GAP
                    if not any(r in why for r in RETRY) or tries[key] >= 10:
                        refused.add(key)
                        print(f"{s} refused {p['id']}: {why}")
        if busy:
            quiet_since = time.time()
        elif time.time() - quiet_since > a.settle:
            break
        time.sleep(2)
    else:
        print(f"timeout pending={sorted(open_ids)}")
        sys.exit(1)
    print(f"approved={approved} refused={len(refused)}")
    if refused:
        sys.exit(1)


def cmd_read(a):
    state = load()
    n = node(state, a.seat)
    sid = secret_of(n, a.name, reader=a.seat, depositor=a.depositor)
    deadline = time.time() + a.timeout
    while True:
        ok, v = n.try_tool("vault_read", {"secret_id": sid})
        if not ok:
            print(f"{a.seat} refused: {v}")
            sys.exit(2)
        if v.get("reply") == "vault_text":
            print(v.get("text", ""))
            return
        if time.time() > deadline:
            die(f"{a.seat}: waiting for answers {v.get('have')}/{v.get('need')}")
        time.sleep(1)


def cmd_stop(a):
    state = load()
    s = state["seats"].get(a.seat) or die(f"unknown seat {a.seat}")
    stop_pid(s.get("pid"))
    print(f"{a.seat} stopped")


def stop_pid(pid):
    if not alive(pid):
        return
    os.kill(pid, signal.SIGTERM)
    deadline = time.time() + 30
    while alive(pid) and time.time() < deadline:
        time.sleep(0.3)
    if alive(pid):
        os.kill(pid, signal.SIGKILL)


def cmd_start(a):
    state = load()
    s = state["seats"].get(a.seat) or die(f"unknown seat {a.seat}")
    if alive(s.get("pid")):
        die(f"{a.seat} is running")
    spawn(state, a.seat)
    save(state)
    n = node(state, a.seat)
    if s.get("workspace"):
        n.wait_session(lambda sv: sv.get("active_workspace") == s["workspace"], "workspace open", 60)
    print(f"{a.seat} started")


def cmd_status(_a):
    state = load()
    for name in headless(state):
        s = state["seats"][name]
        if not alive(s.get("pid")):
            print(f"{name}: down")
            continue
        try:
            v = node(state, name, timeout=5).vault()
        except LabError as e:
            print(f"{name}: up, {e}")
            continue
        print(f"{name}: up workspace={s.get('workspace', '')[:8] or '-'}")
        for d in v.get("deposits", []):
            print(
                f"  {d['name']} by {d['depositor']} {d['state']} "
                f"{d.get('verified', 0)}/{d.get('holders', 0)} readable_by={d.get('readable_by', 0)}"
                + (" reseal" if d.get("reseal") else "")
            )
            for c in d.get("complaints", []):
                print(f"    complaint by {c['holder']} {c['status']}")
        for g in v.get("grants", []):
            print(f"  grant {g['name']} to {g['reader']} {g['state']}")


def cmd_down(_a):
    if os.path.exists(STATE):
        state = load()
        for s in state["seats"].values():
            stop_pid(s.get("pid"))
    shutil.rmtree(LAB, ignore_errors=True)
    print("down")


def main():
    p = argparse.ArgumentParser(description="vault lab: headless seats over MCP")
    sub = p.add_subparsers(dest="cmd", required=True)
    up = sub.add_parser("up")
    up.add_argument("--relay", required=True)
    up.add_argument("--seats", type=int, default=3)
    up.add_argument("--threshold", type=int, default=2)
    up.add_argument("--complainer", default="")
    up.add_argument("--founder-headless", action="store_true")
    sub.add_parser("join")
    s = sub.add_parser("seal")
    s.add_argument("seat")
    s.add_argument("name")
    s.add_argument("text")
    s.add_argument("--kind", default="text")
    g = sub.add_parser("grant")
    g.add_argument("name")
    g.add_argument("reader")
    g.add_argument("--by", default="")
    g.add_argument("--depositor", default="")
    ap = sub.add_parser("approve")
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--settle", type=int, default=15)
    r = sub.add_parser("read")
    r.add_argument("seat")
    r.add_argument("name")
    r.add_argument("--timeout", type=int, default=180)
    r.add_argument("--depositor", default="")
    for verb in ("stop", "start"):
        sub.add_parser(verb).add_argument("seat")
    sub.add_parser("status")
    sub.add_parser("down")
    a = p.parse_args()
    try:
        {
            "up": cmd_up,
            "join": cmd_join,
            "seal": cmd_seal,
            "grant": cmd_grant,
            "approve": cmd_approve,
            "read": cmd_read,
            "stop": cmd_stop,
            "start": cmd_start,
            "status": cmd_status,
            "down": cmd_down,
        }[a.cmd](a)
    except LabError as e:
        die(str(e))


if __name__ == "__main__":
    main()
