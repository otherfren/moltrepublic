# Dynamic Mesh Membership

> **Scope note (2026-07-30, etappe N-demo):** the SMP transport was removed in
> the Nostr transport replacement (`docs/transport/nostr_transport_marmot.md`).
> The queue-ROTATION machinery that reused this announce shape (Stage-3
> deaf-leg rotate, Track C, the nonce'd relayed announce) is deleted and
> historical. The mesh-extension/recovery path this document describes stays
> LIVE behavior over the loopback transport until N4/N5 replace it with the
> Nostr ritual/runtime.

*Status: IMPLEMENTED 2026-07-09 for the recovered-seat case, end to end
(rejoiner `rejoin_mesh`; coordinator window + relay; survivor extension +
supervisor rebuild + grown-mesh persist; rejoiner engine standup). Proven live
in `two_instances.rs` (`recovery_completes_…`, `recovery_distributes_…`,
`a_survivor_folds_…` — the last also pins the §4 queue-rotation shape).
Seat-adding is a **won't-do** (product decision 2026-07-11): membership and
threshold are fixed at founding, forever — the recovered-seat case is the ONLY
mesh-membership change.*

## 1. Problem

The runtime supervisor runs a **fixed peer set**: the full-mesh `PeerLink`s it
was built with (`build_real_net`). Membership changes at runtime — today
concretely: a **recovered seat** that re-entered the MLS group but holds no
mesh links (recovery option A materializes with an empty mesh) — need the mesh
to *grow* on both ends:

- the **rejoiner** needs one per-pair link to every survivor;
- every **survivor** needs one per-pair link to the rejoiner, folded into its
  *running* supervisor.

## 2. Shape: the founding bootstrap, replayed for one seat

The founding already solves "N strangers assemble a full mesh": each node opens
one per-pair inbound queue per peer and broadcasts a **`MeshAnnounce`** — MLS
encrypted, so the *sender is group-authenticated* — with the founder acting as
a temporary relay star. Dynamic membership replays exactly that shape with the
**recovery coordinator as the relay** and the **runtime mesh as the star**:

```
R (rejoiner)                    S (coordinator)              other survivors
────────────                    ───────────────              ───────────────
❶ one fresh inbound queue per
  survivor; MeshAnnounce{peer→
  handover}, MLS-encrypted
  ── RitualMsg::MeshAnnounce(ct) ──▶ ❷ decrypt (sender = R, MLS-
     on the RECOVERY queue              authenticated); relay ct
                                        verbatim over the mesh as
                                        WorkspaceEvent::MeshAnnounced ──▶ ❸ decrypt (sender = R)
                                     ❹ create own inbound queue      ❹ create own inbound queue
                                        for R; reply MeshAnnounce{       for R; reply likewise —
                                        R→handover}, MLS-encrypted,      DIRECTLY onto the queue
  ◀── reply ct, as the FIRST ────       onto R's announced queue         R announced for them
      frame on each announced        ❺ rebuild own supervisor        ❺ rebuild own supervisor
      queue                             with the extended mesh          with the extended mesh
❻ collect every survivor's reply,
  assemble_mesh → PeerLinks →
  RejoinOutcome.mesh → the engine
  stands the runtime net up
```

Key decisions (and why):

- **Replies go DIRECTLY onto the rejoiner's announced queues** — no return
  relay. R's announce hands every survivor a private path to R; per-queue FIFO
  guarantees the reply is the *first* frame on that queue, ahead of any runtime
  traffic, so R reads it before handing the queue to its supervisor (the
  supervisor's cursor then starts after it). This deletes half the founding
  star (the reply-relay) and its failure modes.
- **The relay is the runtime mesh, not a new star.** `WorkspaceEvent::
  MeshAnnounced{ct}` is a transport-only event (`crosses_wire`, no-op apply,
  like `MlsCommit`): the coordinator records it once and the outbox fans it
  out. Survivors act on it; it never becomes history.
- **Authenticity is MLS, end to end.** The announce ct relays *verbatim*;
  whoever decrypts it gets the group-authenticated sender (the rejoiner —
  post-re-key, so only the current seat holder can produce it). A survivor
  never acts on an unauthenticated handover. Replay is dead: an MLS
  application message decrypts once per recipient, at the current epoch.
- **Extending = rebuilding.** A survivor folds the new link in by tearing its
  supervisor down and `build_real_net`-ing with `old mesh + R-link` — the
  exact reopen path (cursors persist per peer in `transport.state`, so nothing
  is lost). Cheap, once per recovery, and no new supervisor surface.
- **Best-effort, like the founding bootstrap.** The rejoiner bounds its wait
  (`MESH_BOOTSTRAP_TIMEOUT`); on timeout it proceeds mesh-less (recovery
  option A still holds — state is already recovered). A survivor that misses
  the announce simply stays unlinked until the next recovery/announce; chat is
  ephemeral by design and the chain has catch-up.

## 3. Guards (hardened by the 2026-07-10 review)

- **Coordinator window.** The coordinator accepts a recovery-queue
  `MeshAnnounce` only for a member whose re-key it just performed (armed in
  `coordinator_rekey`, disarmed when handled) — the recovery queue cannot be
  used to re-point arbitrary members' links. The announce is **parsed before
  the window is spent**: a malformed-but-authentic frame degrades to a drop,
  never burns the rejoiner's only re-mesh chance.
- **Per-member cooldown.** Folding a link in costs every peer a supervisor
  teardown+rebuild+fsync, so an accepted announce stamps a 60 s per-member
  cooldown — the first announce always passes, rapid repeats are ignored
  (bounding the churn a misbehaving member can inflict). A committed
  `Restored` block clears the member's cooldown on every node: a
  threshold-approved re-admission outranks the rate limit.
- **Live roster, not genesis.** The rejoiner's anchor check and survivor set
  come from the **verified chain head** (membership blocks evolve the roster)
  — a post-genesis member can recover, and the announce reaches every live
  seat.
- **Partial mesh.** One silent survivor no longer costs the links that DID
  come back: the timeout assembles the answering subset (mesh-less option A
  only when nobody replied), and a reply must carry a usable handover before
  it counts.
- **Workspace scope.** Recovery recv loops and extension results are scoped
  to the OPEN WORKSPACE (`net_scope`), not to a mesh incarnation — an
  extension's own rebuild cannot orphan an outstanding recovery or a
  concurrent extension; only a workspace switch does.
- **Ratchet continuity.** The extension rebuild hands the LIVE group `Arc`
  through (no snapshot→restore), so a dying outbox's late encrypt can never
  rewind sender generations; the persist snapshot is taken after the rebuild.
- **Persistence.** The extended mesh merges into `transport.state` via a
  LIVE (non-sealing) `MergeCrypto`, and cursor saves merge **only** the
  cursor maps — a stale supervisor clone can never revert the grown mesh.

## 4. What this deliberately does not do

- **Adding brand-new seats** — **won't-do, permanently** (product decision
  2026-07-11): a republic's membership and threshold are fixed at founding
  and never change. `Membership` chain changes cover only the `Restored`
  re-admission of an EXISTING seat; do not design or build a seat-adding
  governance flow.
- **Queue rotation for existing members** — same mechanics, different guard;
  out of scope.
- **Removing seats** — MLS removal + link teardown; separate design (note:
  removal would not change n or the threshold — a removed seat is a dead
  seat, not a smaller republic).

The recovery flow this completes is `recovery_ritual.md`; the founding
bootstrap it mirrors is in `crates/molt-net/src/mesh.rs` + `founding.rs`.
