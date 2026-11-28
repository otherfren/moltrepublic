# Report Seat C - Mr. Right, Run 3

(Verbatim the seat's final message of 17:33 local plus its two addenda; the
"third outage" of the last addendum is the orchestrator's planned shutdown
of all three nodes at the end of the run.)

## 1. Numbers

**Pages:** 125 (14:33:23Z) → **197** (15:33:57Z) on my chain. Higher on the majority chain - five change sets are missing for me (see 4).

**My proposals, run 3: 19.** 11 applied on my chain (150, 153, 156, 168, 171, 174, 177, 183, 186, 189, 192), 4 more only at the others (198, 201, 207, 213), 1 rejected (147, rightly - corrected, went through as 168), 2 withdrawn (216, 219 - duplicates), 2 lost as `superseded` (207, 213), 4 open (198, 201, 222, 225). In addition **4 zombie entries from round 2 withdrawn** (3, 55, 102, 111 - all mine, all dead: their paths had long been in the base, the machine had not marked them `superseded` because they were pure creation patches).

**Approvals: 15** (142, 145, 154, 170, 172, 181, 182, 187, 190, 202, 203, 205, 198, 201, 209). **Rejections: 0** - the target was two. Nothing wrong was submitted to me; rejecting two proposals to meet a quota would have been worse than the gap. For 202 and 203 I ran `wiki_links {direction: in}` before approving and counted that both renames take all referrers along.

**Rename: 1** - `primitive/art.md` → `primitive/asynchronous-ratcheting-trees.md` (proposal 213, 7 edges from 6 pages repaired along). Applied on the majority chain, `superseded` on mine.

**Others' pages: 3 signed sections** (proposal 207): `protokolle/marmot.md` (RFC 9750: DS untrusted by design, AS non-existent in Nostr), `standards/nip-44.md` (defends seat A's header against my own initial suspicion + the HMAC-instead-of-Poly1305 rationale), `standards/xep-0384.md` (OMEMO version history, 17 versions, break on 2020-03-08).

**Disputes: 2 opened and resolved** (proposal 222, undecided): authorship of the ratchet (the specification takes 3 of 4 components, DH ratchet from OTR), scope of "post-quantum" (identity keys are classical even after SPQR).

**Messages run 3:** 134 in total (group 28, patch 86, sync 14, ontologie 3, disputes 3); by me **47** (group 12, patch 28, sync 6, disputes 1). Left 43, Center 44.

## 2. Images and files

| File | share id | cks | Persist | share→persistent | referenced in | `wiki_get.files` | `local.kind` |
|---|---|---|---|---|---|---|---|
| kryptoschicht-stapel.png | 99148677… | 7bc4de227768 | **150 ✓** | 14:45:25→≤14:47:51 = **≤146 s** | kryptoschicht.md (in 225) | - | own |
| abloeseketten.png | fbdc2f47… | d7153750c78a | **153 ✓** | 14:47:43→≤14:51:27 = **≤224 s** | zeitachse.md (in 225) | - | own |
| zeitachse-jahre.png | 2ef39e5b… | dd0cc8f18ae7 | **192 ✓** | 15:05:28→≤15:09:42 = **≤254 s** | **zeitachse.md ✓** | `[{hex dd0cc8f18ae7, name …, state local}]` | own |
| vergleich-balken.png | 6b0c3193… | 41146e6d82f0 | 198 - at YOURS 15:15:00Z, at mine never | open | vergleich.md (in 225) | - | own |
| facetten.csv (725 B) | 97a1bcce… | 5a1adc377442 | 201 - at YOURS 15:15:05Z, at mine never | open | vergleich.md (in 225) | - | own |

**Mirror experiment:** 14:45:47Z persist B-implementierungen.csv → 14:48:02Z `local.kind: none` (0/1) → 14:51:27Z `mirrored` (1/1). **none → mirrored in ≤205 s, without a single call of my own.** `partial` is real and only visible for multi-piece files (B-zeitachse-segment.png: 2/3 → 3/3). Final state 15:23:30Z: 15 files, `used=2113920`, of which 8 foreign `mirrored`, 2 `none` - exactly those whose persist decision is on the majority chain. The mirror fetches only what is persistent.

**Experiment "reference before the vote":** dry_run `warnings: []`, real call `proposed`, `warnings: []` - **neither warning nor rejection**, `resolve_upload` said `temporary: true` at the same time. I never saw `wiki_health.files.temporary` at 1, because the persist decision (15:09) sealed before the text decision (15:12) - the race turned out well, but only through the vote order. Rule: **persist first, reference afterwards.**

**Experiment wrong prefix:** verbatim `"file reference unresolved: upload:deadbeefcafe"`.

## 3. `wiki_props` / `wiki_health`

33 keys, 197 pages. `type` 197 (standard 56, person 43, implementation 43, primitive 13, protocol 12, organization 12, event 9, overview 5, meta 1) · `tags` 967/207 · `first_released` 96/30 · `authored_by` 68/45 · `implements` 58/20 · `notable_year` 45/20 · `status` 31/8 · `disputed_by` 19/14 · `supersedes` 11.

**`wiki_health` final: dangling 0, orphans 15, key_drift 0, `files` 0/0/0.** Two of the orphans are mine (`triple-ratchet-2025.md`, `mlspp.md`) - proposal 222 clears them away.

**Hygiene (171):** seven person pages carried `year` instead of `notable_year` (ontology 4b), all seven from my segment. Before `year` 97/`notable_year` 25, after 90/32. **`wiki_health` does not see this** - `key_drift` only catches spellings, not wrong keys; found with 32 `wiki_get` calls. **Second hygiene round (186):** 6 of 194 pages carried no seat tag, 5 of them my first five from round 1.

## 4. Sync history and DIVERGENZ

`h=59/docs=125` (14:36) → `h=71/135` → **`h=80 last=172 docs=159 rev=11`, identical from me at 14:58:46Z and from seat A at 14:59:06Z** → `h=83/169` → `h=96 last=194 docs=197 rev=26` (15:19:28Z).

**The first deviation was never visible in the sync lines** - the last matching pair was h=80, after that nobody but me wrote one any more. It became visible only like this:

- 15:15:35-15:16:55Z: `./mcp` → `ConnectionRefusedError [Errno 111]`. The orchestrator restarted the node twice, because the engine had sealed a checkpoint at **height 97**, my node co-signed and then rejected the finished block. **The reopen did not catch up 97** - the head stays at 96 until the end.
- **Five open proposals of three seats jumped to `superseded` simultaneously** (202, 203, 205, 207, 213) and lost their votes, my own included. The last sealed proposal 194 shares a path with **none** of them; the same edit lists passed cleanly as `dry_run` unchanged. `superseded` was not a statement about content here.
- **`read_chain.diverged` and `status.chain_diverged` stayed empty throughout.** I recognised it by the `⚖` system lines in my own chat: eight decisions between 15:14:07Z and 15:16:19Z (209, 203, 198, 201, 207, 202, 205, 213), four of them on my own proposals - all `proposed` or `superseded` at my node.
- `DIVERGENZ Mr. Right seit 15:15:35Z (Block 97)` at 15:22:30Z in `sync`, 216 and 219 withdrawn as duplicates.
- **The best available detector is `read_members.last_seen`:** both counterparts silent since 15:16:0x, the age has grown linearly with the clock since. `chain_diverged` reports **contradiction, not silence** - a silent republic looks like a united one to that field. Proposal for the sync line: `seen=<oldest last_seen distance>`.

**U-turn at the end:** I had announced that I would propose nothing more. Then I checked the justification instead of believing it - `wiki_edit {"with_patch": true}` shows five hunks, **none contains a line that the foreign proposal 213 changed**. The patch is disjoint from the divergence, the residual danger is a `refused` on the other side, i.e. noise instead of damage. Proposal **225** (finale: all four images + CSV referenced, correction of the round-2 claim "no two-stage succession chain" - there is one, RFC 2440 → 4880 → 9580 with `via=1` - and the two new disputes) is open. The U-turn stands publicly with its justification in the group chat.

## 5. Friction log

`friction-right.md` (425 lines).

In short, everything about the file/image path is NEW in this run: `share_file` hashes 106 KB in **under 2 s**; `propose files persist` takes the **chat message id**, not the checksum, and the whole grammar is stated only in `read_uploads`/`resolve_upload` - `propose` mentions `files` only in the enum; `availability` (`sharer-only`/`relay-held`) is explained nowhere and **my first interpretation was wrong** (the persistent, triple-mirrored files are on `sharer-only`); `mirror_held/of` counts pieces, `mirrors` counts members. Not new, but newly evidenced: **`rename` does not repair incoming links and does not warn about it**; `withdraw` takes no `note`; the proposal number exists only after `propose` (guessed right three times, wrong once, corrected publicly). **No tool shows the chain height of the others.**

## 6. Collaboration

Clean until the connection broke. Both rejections of 147 were **justified and precise**: seat A found the one wrong line (NIP-44 computes on secp256k1, not Curve25519), seat B the reversed edge direction, which I had done once right and once wrong in the same submission. Both fixed immediately, refiled, passed. Nobody overwrote or bypassed me; I explicitly justified seat B's reprimand of 154 for seven paths during my hygiene round as a rule-7 exception. Only the infrastructure blocked: since 15:16:19Z no sign of life from either, my node is isolated in both directions - my last five proposals cannot reach the threshold of 2 as long as that stays so.

## Addendum 1 (15:39Z) - the transport came back, the chain did not

After 1259 s / 1250 s of silence the `last_seen` values of both counterparts jumped to 2 s and 1 s; 41 chat messages and 18 foreign proposals arrived at once. `read_chain` HEAD unchanged 96, `wiki_list total = 197`, seat A reports 212 pages; `read_chain.diverged` and `status.chain_diverged` still empty.

The sharpest finding: seat B approved my finale 225 with the check "all four upload references are persistent (7bc4de227768, 41146e6d82f0, 5a1adc377442, d7153750c78a)". On MY node two of them (41146e6d82f0, 5a1adc377442) are temporary, because the persist decisions 198 and 201 never arrived here. **The same image reference is clean and unclean on two chains at the same time**, and both nodes report `wiki_health.files = 0/0/0`.

Seven new file proposals signed (218, 220, 223, 226 persist; 229, 232, 235 unpersist of the v1 predecessors), checked before the unpersist votes that no page on my base references the old checksums. Each of these votes is probably ineffective: signatures are position-bound, I sign at 96, the others higher.

Corrected tally: approvals 22; 225 signed by seat B, on their chain; the U-turn on 225 was afterwards shown to be right.

## Addendum 2 (15:51Z-16:08Z) - the node is gone

25 measurements 15:40:41Z-15:51:00Z completely stable: `h=96`, 20 open proposals, 222 and 225 `proposed`. From 15:51:25Z `ConnectionRefusedError` on 127.0.0.1:4042, uninterrupted until 16:08:19Z (this is the orchestrator's planned shutdown of all three nodes). The state "transport back, chain dead" was stable for ten minutes: head 96, `wiki_list` 197, no movement; 22 approvals cast produced not a single block on my chain. 222 and 225 stand on my chain as `proposed` until the end; the only evidence that 225 reached the other side is seat B's approval in the patch channel - chat evidence, not chain evidence.

My work list is empty, the four images and the CSV are drawn, shared and proposed, the three overviews reference all four plus the CSV. On my chain three images of these are persistent and one referenced; on the majority chain everything is complete per seat B's check.

The one finding I put ahead of this run: three outages, one chain split, 40 minutes of divergence - and the entire diagnosis had to come from fields not built for it: `read_members.last_seen` for silence, the `⚖` chat lines for contradiction. The two fields that are built for it, `read_chain.diverged` and `status.chain_diverged`, were empty throughout. **The republic has no working indicator that a seat no longer reads the same chain.**
