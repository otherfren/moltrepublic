# Recovery and catch-up feedback

**Status: EXECUTED, built 2026-11-20 (R1-R3).** User requirements
from a live rejoin that hung after "welcomed back - fetching the chain
anchor" and timed out with "the republic's chain anchor never arrived".

## Requirements (user, 2026-11-20)

1. The rejoiner's step log shows which version the coordinator runs and
   WHY the rejoin failed; the coordinator gets an error in its own log.
2. The other seats see a returning seat's state at its name, with a
   tooltip on how far its catch-up is.
3. Once the anchor and the start of the chain verified, the workspace
   opens (already so: the rejoin materializes on the first verified run;
   everything above arrives by catch-up - kept, pinned by a test).
4. In the workspace, a catch-up indicator with progress and details,
   always visible while this node is more than 3 blocks behind the
   highest height a peer reported.

## R1. Versions and failure reasons

- `RecoverRequest` and `RecoverProgress` gain `#[serde(default)] version`
  (`CARGO_PKG_VERSION`). An empty value means a build before v0.0.18.
- The rejoiner logs `coordinator runs 0.0.19` (or `coordinator runs
  0.0.18 or older`: v0.0.18 sends no version) once its first progress
  frame arrives.
- On the anchor timeout the reason names what arrived: checkpoint parts
  `k/n` or none, blocks received, the last verify error, and, for an old
  coordinator, that it cannot send a checkpoint over one frame.
- New `RitualMsg::RecoverFailed { member, reason, version }`, rejoiner ->
  coordinator, gift-wrapped like the request. The coordinator's recovery
  inboxes turn it into internal `NetRecoverPeerFailed`; the actor logs
  `recovery_failed member= version= reason=` at WARN when the sender is
  the seat's anchor in its latest `Restored` block, or the anchor the
  coordinator holds a pending recovery for. An older coordinator drops
  the unknown variant.

## R2. Catch-up indicator in the workspace

- `SessionView.catchup` (as built; not `status.chain_sync`): head, target
  (highest height a peer on this branch reported), rate (blocks in the
  last minute), an open request, fork candidates.
- The GUI shows a bar above the surface while `target - head > 3`:
  `Catching up: block 412 of 934`, a progress bar, and one detail line
  (open catch-up request, fork candidates, last fork outcome, rate).

## R3. Others see a returning seat

- `MemberInfo` gains `#[serde(default)] sync: String`, engine-made:
  `recovering:<have>:<need>` while a `Restored` proposal for the seat is
  open; `readmitted` after it sealed until the seat is heard; `behind:<h>:<head>`
  while its reported height trails this node's head by more than 3.
- The member chip shows a marker; its tooltip says the state in words.

## Tests

R1: a progress frame without `version` decodes and reads as "older"; the
timeout reason lists parts/blocks; `NetRecoverPeerFailed` logs only from
the seat's anchor. R2/R3: engine values pinned in unit tests; the GUI maps
them (headless).
