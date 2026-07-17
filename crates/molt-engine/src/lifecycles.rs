// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine-run lifecycles: **founding** (create) and **join** are real
//! over SMP — founding provisions invite queues and waits for real members to
//! seal; joining runs the member ritual off the actor and enters the republic
//! once the founder distributes the sealed roster. **restore** is still a mock
//! (its real backup paths are storage milestones S4/S5). They share a
//! [`RunCore`] (step / progress / outcome / log) and cancel-to-choice.

use molt_core::{
    demo_workspace_id, roster_members, Command, CreateState, JoinState, MemberId, MemberInfo,
    MoltError, Reply, RestoreState, RunCore, Screen, SessionScope, WorkspaceId, WorkspaceInfo,
};
use tokio::sync::oneshot;

use crate::{now_secs, ActiveStorage, Envelope, State};

/// Guard shared by every `*Start`: refuse while that run is in flight.
fn guard_idle(run: &RunCore, err: fn(String) -> MoltError) -> Result<(), MoltError> {
    if run.running() {
        return Err(err("already running".to_string()));
    }
    Ok(())
}

/// Guard shared by every `*Tick`: answering with an error stops the ticker.
fn guard_ticking(run: &RunCore, err: fn(String) -> MoltError) -> Result<(), MoltError> {
    if !run.running() {
        return Err(err("idle".to_string()));
    }
    Ok(())
}

/// Guard shared by every `*Finish`: only a successful run finishes.
fn guard_finished(run: &RunCore, err: fn(String) -> MoltError) -> Result<(), MoltError> {
    if run.outcome != 1 {
        return Err(err("not finished successfully".to_string()));
    }
    Ok(())
}

impl State {
    /// Materialize a workspace directory from its founding facts and make it
    /// the actor's open workspace: `molt-storage::create_workspace` writes
    /// manifest, sealed key and the `Founded` genesis frame atomically, the
    /// genesis is applied in memory, and the append side moves onto a writer
    /// task. Shared by the create / join / restore finishes (the joiner and
    /// the restorer materialize their local dir the same way the founder
    /// does). Returns the new workspace id.
    #[allow(clippy::too_many_arguments)]
    fn materialize_workspace(
        &mut self,
        name: &str,
        member: &str,
        rule_m: u8,
        roster: Vec<MemberId>,
        seed_phrase: &str,
        identities: Vec<molt_core::MemberIdentity>,
        attestations: Vec<molt_core::RosterAttestation>,
        republic_id: String,
        agenda: String,
        // this node's ALREADY-derived identity signing key (the ritual anchors
        // it under a workspace-id-derived string, so re-deriving from the member
        // handle here would NOT match the roster — it must be passed in). `None`
        // for paths without a real founding (restore/demo), which have no chain.
        signing_key: Option<molt_storage::SigningKey>,
        mls_snapshot: Option<Vec<u8>>,
        mesh: Vec<molt_core::MeshLink>,
        // a recovery adopts the FULL verified chain it caught up over the
        // recovery channel (the genesis alone would drop every later block's
        // state); `None` = a founding/join, whose chain IS the genesis. A
        // pruned coordinator's serve carries the checkpoint blob the suffix
        // anchors on (WP4b 4c) — persisted next to the chain.
        full_chain: Option<Vec<molt_core::ChainBlock>>,
        checkpoint_blob: Option<molt_core::CheckpointState>,
        err: fn(String) -> MoltError,
    ) -> Result<WorkspaceId, MoltError> {
        let entropy = molt_storage::seed_entropy(seed_phrase).map_err(|e| err(e.to_string()))?;
        let rule_n = u8::try_from(roster.len()).unwrap_or(u8::MAX);
        // one place builds the `Founded` body (SealedRoster::into_genesis) so a
        // new genesis field can't be forgotten between the founder, GUI-join
        // and standalone-join paths
        let sealed = molt_core::SealedRoster {
            name: name.to_string(),
            republic_id,
            rule_m,
            rule_n,
            roster,
            identities,
            attestations,
            agenda,
        };
        let genesis = sealed.into_genesis(member, now_secs());
        // Block 0 of the persistent chain IS the founding: the sealed roster as a
        // Genesis change, signed by the founding attestations. Only a *real*
        // founding (content republic id + full attestations) roots a chain; a
        // pre-ritual/demo materialize leaves the chain empty. The signing key
        // (anchored in the roster) is kept + sealed so the open workspace can
        // co-sign governance and a reopen resumes without the phrase.
        let chain = match full_chain {
            Some(c) => c,
            None => self.genesis_chain(&sealed),
        };
        let sk_bytes = signing_key.as_ref().map(|sk| sk.to_bytes().to_vec());
        let root = self.workspace_root();
        let opened = molt_storage::create_workspace(&root, &entropy, &genesis)
            .map_err(|e| err(e.to_string()))?;
        // seal the node's own MLS group state + assembled mesh into
        // transport.state **durably and synchronously**, before the writer task
        // takes over the file: the group was just born in the ritual and a
        // fire-and-forget save could drop it (queue full / crash) leaving a
        // workspace that can never decrypt. The dir is fresh, so a state carrying
        // the MLS blob + mesh is complete.
        if mls_snapshot.is_some() || !mesh.is_empty() || !chain.is_empty() {
            let ts = molt_core::TransportState {
                mls: mls_snapshot,
                mesh,
                identity_sk: if chain.is_empty() { None } else { sk_bytes },
                ..Default::default()
            };
            opened.write_transport_state(&ts).map_err(|e| err(e.to_string()))?;
        }
        // the genesis chain block goes to its own file, durably, before the
        // writer takes over — same reasoning as the MLS blob above
        if !chain.is_empty() {
            opened
                .write_chain(checkpoint_blob.as_ref(), &chain)
                .map_err(|e| err(e.to_string()))?;
        }
        let id = opened.manifest.workspace.id.clone();
        let dir = opened.dir().to_path_buf();

        // a previously open workspace closes cleanly before the new one
        // takes over the actor state; a demo mesh belongs to the old
        // context and tears down with it
        self.teardown_net();
        self.close_active_storage();
        self.reset_workspace_state();
        self.apply(&genesis);
        self.next_seq = 2;
        // adopt the chain + the runtime signing key (reset cleared them above).
        // A pruned recovery re-anchors on its blob BEFORE adopting — without
        // it, verify_own would run the genesis rules against a suffix and
        // wipe the chain (review finding: the session was chainless until
        // the next reopen)
        if !chain.is_empty() {
            self.identity_sk = signing_key;
            self.checkpoint_blob = checkpoint_blob;
            self.adopt_chain(chain);
            self.note_governance_readiness();
        }
        let prefs = opened.prefs.clone();
        self.active = Some(ActiveStorage {
            id: id.clone(),
            dir,
            prefs,
            handle: molt_storage::start_writer(opened),
        });
        Ok(id)
    }

    /// Add one freshly founded/joined/restored workspace to the session
    /// list — the single construction the three finishes share, so the
    /// entry's shape cannot drift between the paths.
    #[allow(clippy::too_many_arguments)]
    fn push_workspace_entry(
        &mut self,
        id: &WorkspaceId,
        name: &str,
        rule_m: u8,
        rule_n: usize,
        members: Vec<MemberInfo>,
        seed: String,
        net: String,
        s3: bool,
        agenda: String,
    ) {
        if self.session.workspaces.iter().any(|w| w.id == *id) {
            return;
        }
        self.session.workspaces.push(WorkspaceInfo {
            id: id.clone(),
            name: name.to_string(),
            detail: WorkspaceInfo::rule_detail(rule_m, rule_n),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3,
            size_kib: 16,
            last_backup_min: if s3 { 0 } else { WorkspaceInfo::NEVER },
            seed,
            net,
            agenda,
            encrypted: false,
            members,
        });
    }

    // ---- restore -------------------------------------------------------

    pub(crate) fn cmd_restore_start(
        &mut self,
        way: String,
        target: String,
    ) -> Result<Reply, MoltError> {
        guard_idle(&self.session.restore.run, MoltError::Restore)?;
        self.session.restore = RestoreState {
            run: RunCore::started(),
            way,
            target,
        };
        self.session.screen = Screen::Restore;
        self.emit_session(SessionScope::Full);
        self.spawn_ticker(Command::RestoreTick);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_restore_tick(&mut self) -> Result<Reply, MoltError> {
        guard_ticking(&self.session.restore.run, MoltError::Restore)?;
        self.restore_tick();
        self.emit_session(SessionScope::Restore);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_restore_cancel(&mut self) -> Result<Reply, MoltError> {
        self.session.restore = RestoreState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_restore_finish(&mut self) -> Result<Reply, MoltError> {
        guard_finished(&self.session.restore.run, MoltError::Restore)?;
        let name = "Restored Republic".to_string();

        // idempotent: re-running the restore re-opens the already restored
        // workspace instead of piling up fresh directories and entries
        if let Some(existing) = self
            .session
            .workspaces
            .iter()
            .find(|w| w.name == name)
            .map(|w| w.id.clone())
        {
            self.cmd_open_workspace(existing)?;
            self.session.restore = RestoreState::default();
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }

        let member = self.config.member.clone();
        let roster = self.config.members.clone();
        let rule_m = u8::try_from(self.config.threshold.max(1)).unwrap_or(u8::MAX);
        // the fresh dir's phrase, kept for the session entry (empty in demo
        // mode — a demo entry has no on-disk key hierarchy to restore)
        let mut entry_seed = String::new();
        let id = if self.persist {
            // the real restore paths (S4/S5) will rebuild from the backup;
            // until then the restored dir is founded fresh, like a create
            let seed = molt_storage::generate_seed_phrase()
                .map_err(|e| MoltError::Restore(e.to_string()))?;
            entry_seed = seed.clone();
            // restore rebuilds identities from the backup (S4/S5); until
            // then the fresh local dir carries none
            self.materialize_workspace(
                &name,
                &member,
                rule_m,
                roster.clone(),
                &seed,
                Vec::new(),
                Vec::new(),
                String::new(), // restore rebuilds the republic id at S4/S5
                String::new(), // …and the charter with it
                None,          // no chain yet → no signing key (S4/S5)
                None,          // …and the MLS group (S4/S5)
                Vec::new(),    // …and the mesh (S4/S5)
                None,          // no recovered chain — this is the mock restore
                None,          // …and no checkpoint blob either
                MoltError::Restore,
            )?
        } else {
            demo_workspace_id(&name)
        };
        let members = roster_members(&roster, |_| true, "just now");
        self.push_workspace_entry(
            &id,
            &name,
            rule_m,
            roster.len(),
            members,
            entry_seed,
            "tor".to_string(),
            false,
            String::new(), // restore rebuilds the charter at S4/S5
        );
        self.session.active_workspace = id;
        self.session.restore = RestoreState::default();
        // straight into the workspace — no completion-screen stopover
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Whether a restore target looks plausible for its way (the mock
    /// failure rule): S3 is http(s), files end .molt.enc.
    fn restore_target_plausible(way: &str, target: &str) -> bool {
        match way {
            "s3" => target.starts_with("http"),
            _ => target.ends_with(".molt.enc"),
        }
    }

    /// One tick of the mock restore: advance the percentage and append a
    /// way-specific live-log line; flip the outcome at the end (success) or
    /// at ~45 % for implausible targets (timeout).
    fn restore_tick(&mut self) {
        let r = &mut self.session.restore;
        r.run.progress_pct = (r.run.progress_pct + 2).min(100);
        let t = u32::from(r.run.progress_pct / 2);
        if r.run.progress_pct >= 45 && !Self::restore_target_plausible(&r.way, &r.target) {
            r.run.log.push(format!(
                "✗ error: {} unreachable — timeout after 3 retries",
                r.target
            ));
            r.run.log.push("✗ restore failed".to_string());
            r.run.outcome = 2;
            return;
        }
        if r.run.progress_pct >= 100 {
            r.run
                .log
                .push("✓ restore complete — workspace verified".to_string());
            r.run.outcome = 1;
            return;
        }
        if r.run.progress_pct < 30 {
            r.run.log.push(match r.way.as_str() {
                "s3" => format!(
                    "→ https: GET {}/manifest.enc · 200 OK · rtt {} ms",
                    r.target,
                    80 + 7 * t
                ),
                _ => format!("→ fs: open {} · map segment {t}", r.target),
            });
        } else if r.run.progress_pct < 75 {
            r.run.log.push(format!(
                "↓ chunk {}/23 fetched · 128 KiB · sha256 ok",
                t - 14
            ));
        } else if t % 3 == 0 {
            r.run.log.push(format!(
                "→ copy → ~/.moltrepublic/workspaces/restored/chunk-{}.bin",
                t - 37
            ));
        } else {
            r.run.log.push(format!(
                "→ aes-256-gcm: chunk {}/13 decrypted · merkle node ok",
                t - 37
            ));
        }
    }

    // ---- founding (create): the ritual (transport concept §3.3) --------

    pub(crate) fn cmd_create_start(
        &mut self,
        name: String,
        member: String,
        threshold: u8,
        members: u8,
        net: String,
    ) -> Result<Reply, MoltError> {
        guard_idle(&self.session.create.run, MoltError::Create)?;
        let name = name.trim().to_string();
        let member = member.trim().to_string();
        if name.is_empty() {
            return Err(MoltError::Create("the name must not be empty".to_string()));
        }
        if member.is_empty() {
            return Err(MoltError::Create(
                "the handle must not be empty".to_string(),
            ));
        }
        if threshold == 0 || threshold > members || !(2..=13).contains(&members) {
            return Err(MoltError::Create(
                "threshold must be within 1..=members and members within 2..=13".to_string(),
            ));
        }
        // The founder's recovery phrase is real entropy — the workspace id
        // and every key hangs off it. It is shown once during the ritual
        // and never persisted into the shared session of a real workspace.
        let seed =
            molt_storage::generate_seed_phrase().map_err(|e| MoltError::Create(e.to_string()))?;

        // any prior ritual/mesh belongs to a different context
        self.teardown_ritual();
        self.ritual_attestations.clear();
        let links = self
            .start_ritual(&name, &member, threshold, members, &seed)
            .map_err(MoltError::Create)?;

        // the in-app founding is real over SMP — the founder shares the invite
        // links off-band and waits for real members to join. Only the offline
        // sim test seam simulates the other members.
        let simulated = self.ritual_sim;
        let seats = links
            .into_iter()
            .map(|link| molt_core::RitualSeatView {
                link,
                member: String::new(),
                state: 0,
            })
            .collect();
        self.session.create = CreateState {
            run: RunCore::started(),
            name: name.clone(),
            agenda: String::new(),
            can_propose: false,
            member: member.clone(),
            threshold,
            members,
            net,
            seed,
            seats,
            simulated,
        };
        self.session.create.run.log.push(format!(
            "→ ritual opened · {member} (founder) · {threshold}-of-{members} · {} invite(s) minted",
            usize::from(members).saturating_sub(1)
        ));
        if simulated {
            self.session.create.run.log.push(
                "→ SIMULATION — no real network yet (T3): this node auto-activates and \
                 signs for every member. Nothing was shared off-band."
                    .to_string(),
            );
        } else {
            self.session.create.run.log.push(
                "→ share each link off-band, over a private channel — the ritual waits \
                 for members to activate"
                    .to_string(),
            );
        }
        self.session.screen = Screen::Create;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_create_cancel(&mut self) -> Result<Reply, MoltError> {
        // abandoning voids the distributed links; the disk stays untouched
        // unless the ritual already sealed a workspace into being (then it
        // simply stays listed, just not entered)
        self.teardown_ritual();
        self.ritual_attestations.clear();
        self.session.create = CreateState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// When every seat is sealed, the republic is fully constituted:
    /// the founder signs the same table, the genesis is written (carrying
    /// the identity table AND all n attestations), and only now does the
    /// workspace come into being. Idempotent — fires once.
    pub(crate) fn maybe_finalize(&mut self) {
        if self.session.create.run.outcome == 1 {
            return; // already finalized
        }
        // every seat must be sealed (state 2)
        if self.session.create.seats.iter().any(|s| s.state != 2) {
            return;
        }
        let Some(ritual) = self.net_ritual.take() else {
            return;
        };
        let c = self.session.create.clone();
        // finalize can fail (disk); on failure keep the ritual torn down
        // but surface it — a fresh attempt re-mints
        match self.finalize_founding(&c, ritual) {
            Ok(id) => {
                self.session.create.run.outcome = 1;
                self.session.active_workspace = id;
                self.session
                    .create
                    .run
                    .log
                    .push("✓ roster sealed by everyone · workspace created".to_string());
                // auto-enter the republic, exactly like the joiner does on its
                // own seal (cmd_net_join_sealed) — no manual "Enter republic"
                // step. The post-founding mesh comes up in the background; the
                // `create` state is kept (not reset) so the wizard's final log
                // lines (incl. "direct mesh established") still land.
                self.session.screen = Screen::Main;
            }
            Err(e) => {
                self.session.create.run.outcome = 2;
                self.session
                    .create
                    .run
                    .log
                    .push(format!("✗ founding failed: {e}"));
            }
        }
        self.ritual_attestations.clear();
    }

    /// Write the sealed genesis and materialize the workspace (or, on a
    /// storage-less engine, just push a session entry). Returns the id.
    fn finalize_founding(
        &mut self,
        c: &CreateState,
        ritual: crate::founding::RitualRuntime,
    ) -> Result<WorkspaceId, MoltError> {
        // identities in ritual order: founder first, then seats
        let identities = ritual.sealed_identities();
        let roster: Vec<MemberId> = identities.iter().map(|i| i.member.clone()).collect();
        // the neutral, content-derived republic id is the roster salt every
        // member computes identically; the founder signs the same canonical
        // bytes and its attestation leads the list
        let republic_id = ritual.republic_id(&identities);
        // the canonical bytes bind the ratified charter (agenda); the founder's
        // name/agenda are already the final, proposed ones (cmd_create_propose
        // set c.name/c.agenda and the ritual together)
        let table = molt_core::roster_canonical_bytes(
            &republic_id,
            c.threshold,
            c.members,
            &identities,
            &c.agenda,
        );
        let founder_sig = molt_storage::identity_sign(ritual.founder_sk(), &table);
        let mut attestations = vec![molt_core::RosterAttestation {
            member: c.member.clone(),
            sig: founder_sig,
        }];
        attestations.append(&mut self.ritual_attestations.clone());

        // the complete sealed roster every member (founder included) writes
        let sealed = molt_core::SealedRoster {
            name: c.name.clone(),
            republic_id: republic_id.clone(),
            rule_m: c.threshold,
            rule_n: c.members,
            roster: roster.clone(),
            identities: identities.clone(),
            attestations: attestations.clone(),
            agenda: c.agenda.clone(),
        };

        // build the founder's MLS group from every seat's KeyPackage BEFORE
        // touching disk, so a missing/invalid package fails the founding
        // cleanly (only for a persisted founding — the demo has no workspace to
        // hold a group, and its sim members ignore the Welcome anyway).
        let founder_mls = if self.persist {
            Some(ritual.build_founder_mls().map_err(MoltError::Create)?)
        } else {
            None
        };
        // split the founder's live group from the Welcome: the group is
        // snapshotted (sealed into its workspace atomically with the genesis, see
        // materialize_workspace) and — when the mesh bootstrap is on — kept alive
        // to drive the post-founding announcement exchange before its final save
        let (founder_mls_member, welcome) = match founder_mls {
            Some((mls, welcome)) => (Some(mls), welcome),
            None => (None, String::new()),
        };
        let founder_mls_blob = founder_mls_member
            .as_ref()
            .map(|mls| mls.snapshot())
            .transpose()
            .map_err(|e| MoltError::Create(e.to_string()))?;

        // write the FOUNDER's own workspace first. If the disk fails, the
        // founding fails cleanly and no member is left committed to a
        // constitution the founder never persisted (a retry re-mints a fresh
        // founder identity, so distributing first would orphan them).
        let id = if self.persist {
            let id = self.materialize_workspace(
                &c.name,
                &c.member,
                c.threshold,
                roster.clone(),
                &c.seed,
                identities.clone(),
                attestations,
                republic_id.clone(),
                c.agenda.clone(),
                // the founder's identity key, exactly as anchored in the roster
                Some(ritual.founder_sk().clone()),
                founder_mls_blob,
                // the founder's mesh is not known until its bootstrap finishes;
                // it is persisted then, via NetMeshReady
                Vec::new(),
                None, // a founding's chain IS the genesis
                None, // …rooted, never pruned at birth
                MoltError::Create,
            )?;
            self.persist_simulated_members(&id, true);
            id
        } else {
            demo_workspace_id(&c.name)
        };

        // only now distribute the sealed roster + the MLS Welcome to every
        // member so each writes its own workspace (own seed) and enters the
        // group from the same constitution
        if let Ok(json) = serde_json::to_string(&sealed) {
            ritual.distribute_genesis(json, welcome);
        }

        // opt-in: keep the founding star alive and run the founder's post-founding
        // mesh bootstrap; on completion it persists the assembled direct mesh +
        // the post-bootstrap group over the snapshot just written (NetMeshReady).
        if self.persist && self.ritual_bootstrap {
            if let Some(mls) = founder_mls_member {
                let peers: Vec<MemberId> = roster.iter().skip(1).cloned().collect();
                self.spawn_founder_bootstrap(&ritual, mls, c.member.clone(), peers);
            }
        }

        let s3 = self.session.settings.s3_backup;
        if s3 && self.persist {
            self.persist_backup_pref(&id, true);
        }
        // members show live (the ritual just sealed them all)
        let members = roster_members(&roster, |_| true, "just now");
        // the phrase stays in the entry (and on disk, device-sealed —
        // decision 2026-07-15): the Open screen's details panel shows it
        // while the workspace is at-rest-unencrypted
        let seed = c.seed.clone();
        self.push_workspace_entry(
            &id,
            &c.name,
            c.threshold,
            roster.len(),
            members,
            seed,
            c.net.clone(),
            s3,
            c.agenda.clone(),
        );
        Ok(id)
    }

    pub(crate) fn cmd_create_finish(&mut self) -> Result<Reply, MoltError> {
        // "Enter republic" is refused until the ritual sealed a workspace
        // — the engine enforces it for every operator, not just the GUI
        if self.session.create.run.outcome != 1 {
            return Err(MoltError::Create(
                "the founding ritual is not complete — every member must sign first".to_string(),
            ));
        }
        let id = self.session.active_workspace.clone();
        self.session.create = CreateState::default();
        // straight into the new republic — no completion-screen stopover
        self.session.active_workspace = id;
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- join ------------------------------------------------------------

    pub(crate) fn cmd_join_start(
        &mut self,
        invite: String,
        member: String,
    ) -> Result<Reply, MoltError> {
        guard_idle(&self.session.join.run, MoltError::Join)?;
        let member = member.trim().to_string();
        if member.is_empty() {
            return Err(MoltError::Join("the handle must not be empty".to_string()));
        }
        let invite = invite.trim().to_string();
        // a real join needs a link that carries the SMP transport handover —
        // a bare preview link is not joinable
        let Some(inv) = crate::founding::FoundingInvite::parse(&invite) else {
            return Err(MoltError::Join(
                "not a joinable invite link — it carries no transport details".to_string(),
            ));
        };
        // starting a join abandons any founding the user had open — its
        // recv loops must not seal and hijack the session behind our back
        self.teardown_ritual();
        // the joiner's own recovery phrase (shown once during the join); its
        // identity and its own workspace derive from it
        let seed =
            molt_storage::generate_seed_phrase().map_err(|e| MoltError::Join(e.to_string()))?;
        self.join_generation += 1;
        let generation = self.join_generation;
        let mut run = RunCore::started();
        // the join advances the progress bar through its real stages (request →
        // accepted → charter → sealed) so the wizard shows movement, not a
        // frozen 0%
        run.progress_pct = 15;
        run.log.push(
            "→ join request sent over SMP · waiting for every member to seal the roster".to_string(),
        );
        self.session.join = JoinState {
            run,
            invite: invite.clone(),
            member: member.clone(),
            republic: inv.info.republic.clone(),
            rule_m: inv.info.threshold,
            rule_n: inv.info.members,
            inviter: inv.info.inviter.clone(),
            seed: seed.clone(),
            proposed_name: String::new(),
            proposed_agenda: String::new(),
            awaiting_ratify: false,
        };
        self.session.screen = Screen::Join;
        self.emit_session(SessionScope::Full);

        // run the real member ritual off the actor (it waits, possibly long,
        // for the founder to seal); the outcome returns as an internal command
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Join("engine stopped".to_string()));
        };
        // fail-closed: resolve the SMP dialer from settings before spawning the
        // join task. A TorMisconfigured aborts the join with the reason and
        // sets the transport-health pill (T4 §P1/§P6).
        let dialer = match self.resolve_dialer() {
            Ok(dialer) => dialer,
            Err(reason) => {
                self.emit_session(SessionScope::Full);
                return Err(MoltError::Join(reason));
            }
        };
        // the ratification gate: the join task surfaces the founder's proposed
        // charter on `prop` and blocks on `conf` for the joiner's confirm
        // before signing. A forwarder turns the surfaced charter into an
        // internal command so the wizard can show it.
        let (acc_tx, mut acc_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
        let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
        self.join_confirm = Some(conf_tx);
        let ratify = crate::founding::Ratifier {
            accepted: acc_tx,
            proposal: prop_tx,
            confirm: conf_rx,
        };
        // surface the founder's "join accepted" ack as an internal command so
        // the wizard confirms the join landed (before the later charter step)
        let cmd_tx_acc = cmd_tx.clone();
        tokio::spawn(async move {
            if acc_rx.recv().await.is_some() {
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx_acc
                    .send(Envelope {
                        cmd: Command::NetJoinAccepted {
                            generation: Some(generation),
                        },
                        reply,
                    })
                    .await;
            }
        });
        // enable the post-founding mesh bootstrap in the real product flow (the
        // founder does too — see spawn_with_config); off for test seams
        let bootstrap = self.ritual_bootstrap;
        // a fresh transport slot for this join: the task hands its ritual
        // transport back through it (a late fill from a superseded join lands in
        // that join's own, now-orphaned Arc, never this one)
        self.join_transport = std::sync::Arc::new(std::sync::Mutex::new(None));
        let transport_slot = self.join_transport.clone();
        let cmd_tx_fwd = cmd_tx.clone();
        tokio::spawn(async move {
            if let Some((name, agenda)) = prop_rx.recv().await {
                let (reply, _rx) = tokio::sync::oneshot::channel();
                let _ = cmd_tx_fwd
                    .send(Envelope {
                        cmd: Command::NetJoinCharterProposed {
                            name,
                            agenda,
                            generation: Some(generation),
                        },
                        reply,
                    })
                    .await;
            }
        });
        tokio::spawn(async move {
            let cmd = match crate::founding::ritual_join_over_smp(&invite, member, seed, bootstrap, Some(ratify), None, dialer).await {
                Ok(result) => match serde_json::to_string(&result.sealed) {
                    Ok(json) => {
                        // hand the ritual transport back BEFORE reporting the
                        // seal, so cmd_net_join_sealed can reuse it (its Arc owns
                        // the bootstrap queues' receive credentials)
                        if let Ok(mut slot) = transport_slot.lock() {
                            *slot = Some(result.transport);
                        }
                        Command::NetJoinSealed {
                            sealed: json,
                            mls: result.mls_snapshot.map(hex::encode).unwrap_or_default(),
                            mesh: result.mesh,
                            generation: Some(generation),
                        }
                    }
                    Err(e) => Command::NetJoinFailed {
                        error: e.to_string(),
                        generation: Some(generation),
                    },
                },
                Err(e) => Command::NetJoinFailed {
                    error: e,
                    generation: Some(generation),
                },
            };
            let (reply, _rx) = tokio::sync::oneshot::channel();
            let _ = cmd_tx.send(Envelope { cmd, reply }).await;
        });
        Ok(Reply::Ack)
    }

    /// A real SMP join completed: verify came from the off-actor task; write
    /// the joiner's own workspace from its own seed and enter the republic.
    pub(crate) fn cmd_net_join_sealed(
        &mut self,
        sealed: String,
        mls: String,
        mesh: Vec<molt_core::MeshLink>,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        // a cancelled/restarted join bumped the generation — drop stale results
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        let sealed: molt_core::SealedRoster = match serde_json::from_str(&sealed) {
            Ok(s) => s,
            Err(e) => return self.cmd_net_join_failed(format!("decoding sealed roster: {e}"), generation),
        };
        // the off-actor task already verified this, but re-checking here keeps
        // the actor from ever materialising an unverified roster (defence in
        // depth against a forged internal command)
        if let Err(e) = crate::founding::verify_sealed_roster(&sealed) {
            return self.cmd_net_join_failed(e, generation);
        }
        // decode the joiner's own MLS snapshot up front and FAIL the join on a
        // corrupt one — materialising a workspace whose group can never be
        // rehydrated is worse than a clean failure (symmetric with the founder,
        // which fails hard on a bad KeyPackage)
        let mls_blob = if mls.is_empty() {
            None
        } else {
            match hex::decode(&mls) {
                Ok(blob) => Some(blob),
                Err(e) => return self.cmd_net_join_failed(format!("decoding MLS snapshot: {e}"), generation),
            }
        };
        let j = self.session.join.clone();
        // keep copies to stand the runtime supervisor up after materialising
        let net_seed = (mls_blob.clone(), mesh.clone());
        let id = if self.persist {
            // materialising can fail on disk; a bare `?` would drop the error
            // into the (already discarded) reply channel and hang the join at
            // "in progress" — surface it into the run instead
            // the joiner's identity key, derived exactly as the ritual anchored
            // it (shared helper), so the chain signing key matches the roster
            let joiner_sk = crate::founding::member_identity(&j.seed)
                .ok()
                .map(|(sk, _)| sk);
            match self.materialize_workspace(
                &sealed.name,
                &j.member,
                sealed.rule_m,
                sealed.roster.clone(),
                &j.seed,
                sealed.identities.clone(),
                sealed.attestations.clone(),
                sealed.republic_id.clone(),
                sealed.agenda.clone(),
                joiner_sk,
                mls_blob,
                mesh,
                None, // a join's chain IS the genesis
                None, // …rooted, never pruned at birth
                MoltError::Join,
            ) {
                Ok(id) => id,
                Err(e) => return self.cmd_net_join_failed(e.to_string(), generation),
            }
        } else {
            demo_workspace_id(&sealed.name)
        };
        // the join is done — advance the incarnation so a late result from the
        // (finished) join task can't retroactively touch the reset run
        self.join_generation += 1;
        self.join_confirm = None;
        let members = roster_members(&sealed.roster, |_| true, "just now");
        self.session.join = JoinState::default();
        self.push_workspace_entry(
            &id,
            &sealed.name,
            sealed.rule_m,
            sealed.roster.len(),
            members,
            j.seed.clone(),
            "tor".to_string(),
            self.session.settings.s3_backup,
            sealed.agenda.clone(),
        );
        self.session.active_workspace = id;
        // stand the runtime supervisor up over the joiner's direct mesh, REUSING
        // the transport the ritual ran over (its Arc owns the bootstrap queues'
        // receive credentials — a fresh transport could send but never subscribe).
        // best-effort — no mesh, no transport, or a loopback mesh just means no
        // live peer link yet.
        let (mls_blob, mesh) = net_seed;
        let reused = self.join_transport.lock().ok().and_then(|mut s| s.take());
        if self.persist && !mesh.is_empty() {
            if let (Some(blob), Some(transport)) = (mls_blob, reused) {
                if let Some(net) = self.build_real_net(transport, &mesh, &blob) {
                    self.teardown_net();
                    self.net = Some(net);
                }
            }
        }
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- recover (total-loss rejoin over a molt://recover/… link) --------

    /// Begin recovering a lost seat from a coordinator-minted recovery link
    /// and the seat's recovery phrase — the rejoiner side of the recovery
    /// ritual (`recovery_ritual.md` §4), mirroring [`State::cmd_join_start`].
    /// The rejoin runs off the actor (it waits for the coordinator's threshold
    /// re-admission + Welcome, possibly long); the outcome returns as
    /// [`Command::NetRecoverSealed`] / [`Command::NetRecoverFailed`].
    pub(crate) fn cmd_recover_start(
        &mut self,
        link: String,
        phrase: String,
    ) -> Result<Reply, MoltError> {
        let link = link.trim().to_string();
        // an actionable recovery link carries the coordinator's transport
        // handover — a bare preview link cannot reach anyone
        let Some(inv) = crate::recovery::RecoveryInvite::parse(&link) else {
            return Err(MoltError::Recover(
                "not an actionable recovery link — it carries no transport details".to_string(),
            ));
        };
        // a recovered republic only exists as a materialized workspace — a
        // storage-less node has nowhere to put the verified chain
        if !self.persist {
            return Err(MoltError::Recover(
                "this node has no workspace storage to recover into".to_string(),
            ));
        }
        let phrase = phrase.trim().to_string();
        if phrase.is_empty() {
            return Err(MoltError::Recover(
                "the recovery phrase must not be empty".to_string(),
            ));
        }
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Recover("engine stopped".to_string()));
        };
        // a restarted recovery supersedes the one in flight — bump the
        // incarnation so the stale task's late result is dropped
        self.recover_generation += 1;
        let generation = self.recover_generation;
        self.recover_ctx = Some((inv.clone(), phrase.clone()));
        // a fresh transport slot for this recovery: the task parks its SMP
        // transport here BEFORE the rejoin, so cmd_net_recover_sealed can
        // stand the runtime supervisor up over the re-established mesh (the
        // slot's Arc owns the fresh queues' receive credentials — the
        // join_transport twin)
        self.recover_transport = std::sync::Arc::new(std::sync::Mutex::new(None));
        let transport_slot = self.recover_transport.clone();
        // fail-closed: resolve the SMP dialer before spawning the rejoin task.
        // A TorMisconfigured aborts the recovery with the reason and sets the
        // transport-health pill (T4 §P1/§P6).
        let dialer = match self.resolve_dialer() {
            Ok(dialer) => dialer,
            Err(reason) => {
                self.emit_session(SessionScope::Full);
                return Err(MoltError::Recover(reason));
            }
        };
        self.session.notice = format!("recover-started:{}", inv.member);
        self.emit_session(SessionScope::Full);
        tokio::spawn(async move {
            let cmd = match crate::recovery::transport_for(&inv, dialer) {
                Ok(transport) => {
                    if let Ok(mut slot) = transport_slot.lock() {
                        *slot = Some(transport.clone());
                    }
                    match crate::recovery::run_rejoin(transport, inv, &phrase, true).await {
                        Ok(outcome) => match match &outcome.checkpoint_blob {
                            Some(blob) => {
                                serde_json::to_string(&crate::chain::ServedChainWire::Pruned {
                                    checkpoint_blob: blob.clone(),
                                    blocks: outcome.chain.clone(),
                                })
                            }
                            None => serde_json::to_string(&outcome.chain),
                        } {
                            Ok(chain) => Command::NetRecoverSealed {
                                member: outcome.member,
                                chain,
                                mls: hex::encode(&outcome.mls_snapshot),
                                mesh: outcome.mesh,
                                generation: Some(generation),
                            },
                            Err(e) => Command::NetRecoverFailed {
                                error: e.to_string(),
                                generation: Some(generation),
                            },
                        },
                        Err(e) => Command::NetRecoverFailed {
                            error: e,
                            generation: Some(generation),
                        },
                    }
                }
                Err(e) => Command::NetRecoverFailed {
                    error: e,
                    generation: Some(generation),
                },
            };
            let (reply, _rx) = oneshot::channel();
            let _ = cmd_tx.send(crate::Envelope { cmd, reply }).await;
        });
        Ok(Reply::Ack)
    }

    /// The off-actor rejoin task finished: the seat is back inside the MLS
    /// group and holds the coordinator-served chain, verified from its
    /// genesis. The actor **re-verifies everything** before materializing
    /// (defence in depth against a forged internal command — symmetric with
    /// [`State::cmd_net_join_sealed`]), then writes the recovered workspace,
    /// adopting the FULL chain, and enters the republic. Option A: no live
    /// mesh yet — re-meshing is the separate dynamic-membership feature.
    pub(crate) fn cmd_net_recover_sealed(
        &mut self,
        member: String,
        chain: String,
        mls: String,
        mesh: Vec<molt_core::MeshLink>,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        // a cancelled/restarted recovery bumped the generation — drop stale results
        if generation != Some(self.recover_generation) {
            return Ok(Reply::Ack);
        }
        let Some((inv, phrase)) = self.recover_ctx.clone() else {
            return Ok(Reply::Ack);
        };
        let wire: crate::chain::ServedChainWire = match serde_json::from_str(&chain) {
            Ok(b) => b,
            Err(e) => {
                return self
                    .cmd_net_recover_failed(format!("decoding the recovered chain: {e}"), generation)
            }
        };
        let (blocks, checkpoint_blob) = match wire {
            crate::chain::ServedChainWire::Full(blocks) => (blocks, None),
            crate::chain::ServedChainWire::Pruned {
                checkpoint_blob,
                blocks,
            } => (blocks, Some(checkpoint_blob)),
        };
        // full chain: verified from block 0; pruned: the suffix rules run
        // against the blob (founding-bound anchor, double-apply seed)
        let head = match &checkpoint_blob {
            None => match crate::chain::verify_chain(&blocks) {
                Ok(h) => h,
                Err(e) => return self.cmd_net_recover_failed(e, generation),
            },
            Some(blob) => {
                match crate::chain::verify_suffix_chain(blob, &blocks, &inv.republic_id) {
                    Ok(h) => h,
                    Err(e) => return self.cmd_net_recover_failed(e, generation),
                }
            }
        };
        // the constitution the workspace materializes from: the genesis, or
        // the blob's rid-bound founding table on a pruned serve
        let sealed = match &checkpoint_blob {
            None => blocks.first().and_then(crate::recovery::sealed_roster_from_genesis),
            Some(blob) => Some(crate::recovery::sealed_roster_from_blob(blob)),
        };
        let Some(sealed) = sealed
        else {
            return self.cmd_net_recover_failed(
                "the recovered chain does not root on a genesis constitution".to_string(),
                generation,
            );
        };
        // the chain must be THIS recovery's republic (no swapping in another)
        if head.republic_id != inv.republic_id || member != inv.member {
            return self.cmd_net_recover_failed(
                "the recovered chain does not match the recovery link".to_string(),
                generation,
            );
        }
        // the seat's identity re-derives from the phrase exactly as the ritual
        // anchored it; the VERIFIED HEAD must anchor it (a Membership block may
        // have evolved the key past the genesis — e.g. this very re-admission)
        let (sk, pk) = match crate::founding::member_identity(&phrase) {
            Ok(kp) => kp,
            Err(e) => return self.cmd_net_recover_failed(e, generation),
        };
        if !head.identities.iter().any(|i| i.member == member && i.identity_pk == pk) {
            return self.cmd_net_recover_failed(
                "the recovered chain does not anchor this phrase's identity".to_string(),
                generation,
            );
        }
        // a corrupt MLS snapshot fails the recovery — materializing a workspace
        // whose group can never decrypt is worse than a clean failure
        let mls_blob = if mls.is_empty() {
            None
        } else {
            match hex::decode(&mls) {
                Ok(blob) => Some(blob),
                Err(e) => {
                    return self
                        .cmd_net_recover_failed(format!("decoding MLS snapshot: {e}"), generation)
                }
            }
        };
        // keep copies to stand the runtime supervisor up after materialising
        let net_seed = (mls_blob.clone(), mesh.clone());
        let id = match self.materialize_workspace(
            &sealed.name,
            &member,
            sealed.rule_m,
            sealed.roster.clone(),
            &phrase,
            sealed.identities.clone(),
            sealed.attestations.clone(),
            sealed.republic_id.clone(),
            sealed.agenda.clone(),
            Some(sk),
            mls_blob,
            mesh, // the re-established mesh (empty = option A, state only)
            Some(blocks),
            checkpoint_blob.clone(),
            MoltError::Recover,
        ) {
            Ok(id) => id,
            Err(e) => return self.cmd_net_recover_failed(e.to_string(), generation),
        };
        // the recovery is done — advance the incarnation so a late result from
        // the (finished) rejoin task can't touch the recovered state
        self.recover_generation += 1;
        self.recover_ctx = None;
        let members = roster_members(&sealed.roster, |_| true, "just now");
        self.push_workspace_entry(
            &id,
            &sealed.name,
            sealed.rule_m,
            sealed.roster.len(),
            members,
            phrase.clone(),
            "tor".to_string(),
            self.session.settings.s3_backup,
            sealed.agenda.clone(),
        );
        self.session.active_workspace = id;
        // stand the runtime supervisor up over the re-established mesh, REUSING
        // the rejoin transport (its Arc owns the fresh mesh queues' receive
        // credentials — the join-tail twin). Best-effort: no mesh or no
        // transport just means no live links yet (option A still holds).
        let (mls_blob, mesh) = net_seed;
        let reused = self.recover_transport.lock().ok().and_then(|mut s| s.take());
        if self.persist && !mesh.is_empty() {
            if let (Some(blob), Some(transport)) = (mls_blob, reused) {
                if let Some(net) = self.build_real_net(transport, &mesh, &blob) {
                    self.teardown_net();
                    self.net = Some(net);
                }
            }
        }
        self.session.notice = format!("recovered:{member}");
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A total-loss recovery failed: surface the reason to the operator. The
    /// in-flight context is kept, so a slow legitimate result racing a
    /// transport error can still land (retry = a fresh `RecoverStart`).
    pub(crate) fn cmd_net_recover_failed(
        &mut self,
        error: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.recover_generation) {
            return Ok(Reply::Ack);
        }
        tracing::warn!(error = %error, "recovery failed");
        self.session.notice = format!("recover-failed:{error}");
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// A real SMP join failed: surface the reason in the join run (retryable).
    pub(crate) fn cmd_net_join_failed(
        &mut self,
        error: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        self.session.join.run.outcome = 2;
        self.session.join.awaiting_ratify = false;
        self.join_confirm = None;
        self.session.join.run.log.push(format!("✗ join failed: {error}"));
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_join_cancel(&mut self) -> Result<Reply, MoltError> {
        // invalidate any in-flight join task so its late result is dropped;
        // dropping join_confirm closes the gate → a paused ritual declines
        self.join_generation += 1;
        self.join_confirm = None;
        self.session.join = JoinState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The off-actor join task reached the ratification step: surface the
    /// founder's proposed charter so the wizard can show it and the joiner can
    /// confirm or decline (internal — driven by the join task).
    /// The founder acknowledged the join. Confirm it on the join wizard so the
    /// joiner sees progress while it waits for the deliberation (advisory — the
    /// join still proceeds through the charter + seal). No-op for a stale/idle
    /// join, or once the wizard has already moved past the initial wait.
    pub(crate) fn cmd_net_join_accepted(&mut self, generation: Option<u64>) -> Result<Reply, MoltError> {
        if generation != Some(self.join_generation)
            || self.session.join.run.outcome != 0
            || self.session.join.awaiting_ratify
        {
            return Ok(Reply::Ack);
        }
        let line = "✓ the founder accepted your join · waiting for the deliberation".to_string();
        if self.session.join.run.log.last() == Some(&line) {
            return Ok(Reply::Ack); // idempotent (a resend must not stack lines)
        }
        self.session.join.run.progress_pct = 45;
        self.session.join.run.log.push(line);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_net_join_charter_proposed(
        &mut self,
        name: String,
        agenda: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        self.session.join.proposed_name = name.clone();
        self.session.join.proposed_agenda = agenda;
        self.session.join.awaiting_ratify = true;
        self.session.join.run.progress_pct = 70;
        self.session
            .join
            .run
            .log
            .push(format!("→ charter proposed: “{name}” · review and confirm to join"));
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The joiner ratifies the proposed charter: release the seal signature so
    /// the join proceeds (co-equal — an operator or the GUI). No-op unless a
    /// join is actually paused awaiting ratification.
    pub(crate) fn cmd_join_confirm_charter(&mut self) -> Result<Reply, MoltError> {
        if !self.session.join.awaiting_ratify {
            return Err(MoltError::Join(
                "no charter is awaiting your ratification".to_string(),
            ));
        }
        if let Some(tx) = self.join_confirm.take() {
            // the paused ritual task is waiting on this; a full/closed channel
            // just means it already moved on
            let _ = tx.try_send(true);
        }
        self.session.join.awaiting_ratify = false;
        self.session.join.run.progress_pct = 88;
        self.session
            .join
            .run
            .log
            .push("✓ you ratified the charter · sealing your signature".to_string());
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The joiner declines the proposed charter: release the gate with a `false`
    /// so the ritual task tells the founder it declined (its seat shows declined
    /// on the founder's side) and the join ends as failed. Co-equal. No-op unless
    /// a join is actually paused awaiting ratification.
    pub(crate) fn cmd_join_decline_charter(&mut self) -> Result<Reply, MoltError> {
        if !self.session.join.awaiting_ratify {
            return Err(MoltError::Join(
                "no charter is awaiting your ratification".to_string(),
            ));
        }
        if let Some(tx) = self.join_confirm.take() {
            let _ = tx.try_send(false);
        }
        self.session.join.awaiting_ratify = false;
        self.session
            .join
            .run
            .log
            .push("✗ you declined the charter".to_string());
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- the shared self-ticker ------------------------------------------

    /// Drive a mock run from the engine itself (co-equal: the run makes
    /// progress no matter which operator started it, GUI attached or not).
    /// `tick` is re-sent every 90 ms; the task stops as soon as a tick is
    /// answered with an error (the run is over or was cancelled).
    pub(crate) fn spawn_ticker(&self, tick: Command) {
        // upgrade the actor's weak self-handle; a stopping actor spawns no ticker
        let Some(tx) = self.cmd_tx.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(90)).await;
                let (reply, rx) = oneshot::channel();
                if tx
                    .send(Envelope {
                        cmd: tick.clone(),
                        reply,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                match rx.await {
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
        });
    }
}
