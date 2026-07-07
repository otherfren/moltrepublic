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
        mls_snapshot: Option<Vec<u8>>,
        err: fn(String) -> MoltError,
    ) -> Result<WorkspaceId, MoltError> {
        let entropy = molt_storage::seed_entropy(seed_phrase).map_err(|e| err(e.to_string()))?;
        let rule_n = u8::try_from(roster.len()).unwrap_or(u8::MAX);
        // one place builds the `Founded` body (SealedRoster::into_genesis) so a
        // new genesis field can't be forgotten between the founder, GUI-join
        // and standalone-join paths
        let genesis = molt_core::SealedRoster {
            name: name.to_string(),
            republic_id,
            rule_m,
            rule_n,
            roster,
            identities,
            attestations,
            agenda,
        }
        .into_genesis(member, now_secs());
        let root = self.workspace_root();
        let opened = molt_storage::create_workspace(&root, &entropy, &genesis)
            .map_err(|e| err(e.to_string()))?;
        // seal the node's own MLS group state into transport.state **durably and
        // synchronously**, before the writer task takes over the file: the group
        // was just born in the ritual and a fire-and-forget save could drop it
        // (queue full / crash) leaving a workspace that can never decrypt. The
        // dir is fresh, so a state carrying just the MLS blob is complete.
        if let Some(blob) = mls_snapshot {
            let ts = molt_core::TransportState {
                mls: Some(blob),
                ..Default::default()
            };
            opened.write_transport_state(&ts).map_err(|e| err(e.to_string()))?;
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
        let id = if self.persist {
            // the real restore paths (S4/S5) will rebuild from the backup;
            // until then the restored dir is founded fresh, like a create
            let seed = molt_storage::generate_seed_phrase()
                .map_err(|e| MoltError::Restore(e.to_string()))?;
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
                None,          // …and the MLS group (S4/S5)
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
            String::new(),
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
    /// failure rule): peers speak smp://, S3 is http(s), files end .molt.enc.
    fn restore_target_plausible(way: &str, target: &str) -> bool {
        match way {
            "peer" => target.starts_with("smp://") && target.len() > 8,
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
                "peer" => format!("→ smp: tor circuit hop {t} · handshake {}", r.target),
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
        // the founder's own group snapshot, sealed into its workspace atomically
        // with the genesis (see materialize_workspace)
        let founder_mls_blob = founder_mls
            .as_ref()
            .map(|(mls, _)| mls.snapshot())
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
                founder_mls_blob,
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
        let welcome = founder_mls.map(|(_, w)| w).unwrap_or_default();
        if let Ok(json) = serde_json::to_string(&sealed) {
            ritual.distribute_genesis(json, welcome);
        }

        let s3 = self.session.settings.s3_backup;
        if s3 && self.persist {
            self.persist_backup_pref(&id, true);
        }
        // members show live (the ritual just sealed them all)
        let members = roster_members(&roster, |_| true, "just now");
        let seed = if self.persist { String::new() } else { c.seed.clone() };
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
        // the ratification gate: the join task surfaces the founder's proposed
        // charter on `prop` and blocks on `conf` for the joiner's confirm
        // before signing. A forwarder turns the surfaced charter into an
        // internal command so the wizard can show it.
        let (prop_tx, mut prop_rx) = tokio::sync::mpsc::channel::<(String, String)>(1);
        let (conf_tx, conf_rx) = tokio::sync::mpsc::channel::<bool>(1);
        self.join_confirm = Some(conf_tx);
        let ratify = crate::founding::Ratifier {
            proposal: prop_tx,
            confirm: conf_rx,
        };
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
            let cmd = match crate::founding::ritual_join_over_smp(&invite, member, seed, Some(ratify), None).await {
                Ok(result) => match serde_json::to_string(&result.sealed) {
                    Ok(json) => Command::NetJoinSealed {
                        sealed: json,
                        mls: result.mls_snapshot.map(hex::encode).unwrap_or_default(),
                        generation: Some(generation),
                    },
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
        let id = if self.persist {
            // materialising can fail on disk; a bare `?` would drop the error
            // into the (already discarded) reply channel and hang the join at
            // "in progress" — surface it into the run instead
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
                mls_blob,
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
            String::new(),
            "tor".to_string(),
            self.session.settings.s3_backup,
            sealed.agenda.clone(),
        );
        self.session.active_workspace = id;
        self.session.screen = Screen::Main;
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
