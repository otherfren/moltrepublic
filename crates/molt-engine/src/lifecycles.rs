// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine-run mock lifecycles: restore, founding (create) and join.
//! They share one skeleton — a [`RunCore`] (step / progress / outcome /
//! log), a 90 ms self-ticker, cancel-to-choice and finish-into-workspace —
//! and differ only in validation, log content and the finished workspace.

use molt_core::{
    demo_workspace_id, mock_ticket, roster_members, Command, CreateState, EventEnvelope,
    InviteInfo, JoinState, MemberId, MemberInfo, MoltError, Reply, RestoreState, RunCore, Screen,
    SessionScope, WorkspaceEvent, WorkspaceId, WorkspaceInfo,
};
use tokio::sync::oneshot;

use crate::{entropy_for, now_secs, ActiveStorage, Envelope, State};

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
    fn materialize_workspace(
        &mut self,
        name: &str,
        member: &str,
        rule_m: u8,
        roster: Vec<MemberId>,
        seed_phrase: &str,
        err: fn(String) -> MoltError,
    ) -> Result<WorkspaceId, MoltError> {
        let entropy = molt_storage::seed_entropy(seed_phrase).map_err(|e| err(e.to_string()))?;
        let rule_n = u8::try_from(roster.len()).unwrap_or(u8::MAX);
        let genesis = EventEnvelope {
            seq: 1,
            ts: now_secs(),
            by: member.to_string(),
            body: WorkspaceEvent::Founded {
                name: name.to_string(),
                rule_m,
                rule_n,
                member: member.to_string(),
                roster,
            },
        };
        let root = self.workspace_root();
        let opened = molt_storage::create_workspace(&root, &entropy, &genesis)
            .map_err(|e| err(e.to_string()))?;
        let id = opened.manifest.workspace.id.clone();
        let dir = opened.dir().to_path_buf();

        // a previously open workspace closes cleanly before the new one
        // takes over the actor state
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
            self.materialize_workspace(
                &name,
                &member,
                rule_m,
                roster.clone(),
                &seed,
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

    // ---- founding (create) ---------------------------------------------

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
        // Display names may repeat (the workspace id disambiguates), so
        // there is deliberately no unique-name check here.
        //
        // The founding result is derived up front; the GUI reveals it only
        // once the run succeeds. The recovery phrase is real entropy — the
        // whole key hierarchy of the workspace hangs off it.
        let entropy = entropy_for(&name);
        let seed =
            molt_storage::generate_seed_phrase().map_err(|e| MoltError::Create(e.to_string()))?;
        let invites: Vec<String> = (1..members)
            .map(|k| {
                InviteInfo {
                    republic: name.clone(),
                    threshold,
                    members,
                    inviter: member.clone(),
                    ticket: mock_ticket(entropy.wrapping_add(u64::from(k))),
                }
                .render()
            })
            .collect();
        self.session.create = CreateState {
            run: RunCore::started(),
            name,
            member,
            threshold,
            members,
            net,
            seed,
            invites,
        };
        self.session.screen = Screen::Create;
        self.emit_session(SessionScope::Full);
        self.spawn_ticker(Command::CreateTick);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_create_tick(&mut self) -> Result<Reply, MoltError> {
        guard_ticking(&self.session.create.run, MoltError::Create)?;
        self.create_tick();
        self.emit_session(SessionScope::Create);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_create_cancel(&mut self) -> Result<Reply, MoltError> {
        self.session.create = CreateState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_create_finish(&mut self) -> Result<Reply, MoltError> {
        guard_finished(&self.session.create.run, MoltError::Create)?;
        let c = self.session.create.clone();
        // roster: the founder plus one named seat per unused invite
        let roster: Vec<MemberId> = std::iter::once(c.member.clone())
            .chain((1..c.members).map(|k| format!("invite-{k}")))
            .collect();
        let id = if self.persist {
            self.materialize_workspace(
                &c.name,
                &c.member,
                c.threshold,
                roster.clone(),
                &c.seed,
                MoltError::Create,
            )?
        } else {
            demo_workspace_id(&c.name)
        };
        // the run state resets only once the workspace exists — a failed
        // finish stays retryable, and the recovery phrase the wizard
        // revealed keeps addressing something
        self.session.create = CreateState::default();
        let s3 = self.session.settings.s3_backup;
        if s3 && self.persist {
            self.persist_backup_pref(&id, true);
        }
        // The founder is synced; every invite is still an unused seat. On a
        // persisted workspace the recovery phrase is NOT kept in the shared
        // session (read_session would hand the root of the key hierarchy to
        // every operator); it was shown once, in the wizard.
        let members = roster_members(&roster, |m| m == c.member, "unused");
        let seed = if self.persist {
            String::new()
        } else {
            c.seed.clone()
        };
        self.push_workspace_entry(
            &id,
            &c.name,
            c.threshold,
            usize::from(c.members),
            members,
            seed,
            c.net.clone(),
            s3,
        );
        self.session.active_workspace = id;
        // straight into the new republic — no completion-screen stopover
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// One tick of the mock founding: advance the percentage and append a
    /// phase-specific live-log line; the founding always succeeds (invalid
    /// configurations are rejected at `CreateStart`).
    fn create_tick(&mut self) {
        let dir = self.session.settings.workspace_dir.clone();
        let c = &mut self.session.create;
        c.run.progress_pct = (c.run.progress_pct + 3).min(100);
        let t = u32::from(c.run.progress_pct / 3);
        if c.run.progress_pct >= 100 {
            c.run.log.push(format!(
                "✓ republic founded — {}-of-{} · {} invite(s) minted",
                c.threshold,
                c.members,
                c.members - 1
            ));
            c.run.outcome = 1;
            return;
        }
        if c.run.progress_pct < 30 {
            if t % 2 == 0 {
                c.run
                    .log
                    .push(format!("→ rng: entropy pool block {t} · ok"));
            } else {
                c.run
                    .log
                    .push(format!("→ kdf: argon2id pass {t} · 256 MiB"));
            }
        } else if c.run.progress_pct < 75 {
            let share = t % u32::from(c.members) + 1;
            c.run.log.push(format!(
                "→ frost: share {share}/{} committed · vss ok",
                c.members
            ));
        } else if t % 2 == 0 {
            // the same slug rule the real directory gets (short-id elided —
            // it only exists once the seed is committed at finish)
            let slug = molt_storage::slugify(&c.name);
            c.run
                .log
                .push(format!("→ ws: {dir}/{slug}.…/ manifest sealed"));
        } else {
            // members is validated to 2..=13, so there is at least 1 invite
            let ticket = t % u32::from(c.members - 1) + 1;
            c.run
                .log
                .push(format!("→ invite: ticket {ticket} minted · unused"));
        }
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
        if invite.is_empty() {
            return Err(MoltError::Join("the invite must not be empty".to_string()));
        }
        // Any non-empty invite is accepted for now (real validation comes
        // with the network implementation). A well-formed molt:// link
        // contributes the republic's details; anything else joins under
        // fallback values.
        let info = InviteInfo::parse(&invite);
        self.session.join = JoinState {
            run: RunCore::started(),
            invite,
            member,
            republic: info
                .as_ref()
                .map(|i| i.republic.clone())
                .unwrap_or_else(|| "Joined Republic".to_string()),
            rule_m: info.as_ref().map(|i| i.threshold).unwrap_or(2),
            rule_n: info.as_ref().map(|i| i.members).unwrap_or(3),
            inviter: info.as_ref().map(|i| i.inviter.clone()).unwrap_or_default(),
        };
        self.session.screen = Screen::Join;
        self.emit_session(SessionScope::Full);
        self.spawn_ticker(Command::JoinTick);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_join_tick(&mut self) -> Result<Reply, MoltError> {
        guard_ticking(&self.session.join.run, MoltError::Join)?;
        self.join_tick();
        self.emit_session(SessionScope::Join);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_join_cancel(&mut self) -> Result<Reply, MoltError> {
        self.session.join = JoinState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_join_finish(&mut self) -> Result<Reply, MoltError> {
        guard_finished(&self.session.join.run, MoltError::Join)?;
        let j = self.session.join.clone();
        // Inviter and joiner are synced; the rest of the roster is only
        // learned as those members come online.
        let mut roster: Vec<MemberId> = Vec::new();
        if !j.inviter.is_empty() {
            roster.push(j.inviter.clone());
        }
        roster.push(j.member.clone());
        for k in 3..=j.rule_n {
            roster.push(format!("member-{k}"));
        }
        let id = if self.persist {
            // until the network exists, the "received" group history is a
            // fresh local genesis under a fresh seed (the joiner keeps no
            // recovery phrase — restoring is the founder's power)
            let seed = molt_storage::generate_seed_phrase()
                .map_err(|e| MoltError::Join(e.to_string()))?;
            self.materialize_workspace(
                &j.republic,
                &j.member,
                j.rule_m,
                roster.clone(),
                &seed,
                MoltError::Join,
            )?
        } else {
            demo_workspace_id(&j.republic)
        };
        // reset only after the fallible part — a failed finish stays retryable
        self.session.join = JoinState::default();
        let members = roster_members(
            &roster,
            |m| m == j.member || (!j.inviter.is_empty() && m == j.inviter),
            "not seen yet",
        );
        // a joiner holds no group recovery phrase (yet)
        self.push_workspace_entry(
            &id,
            &j.republic,
            j.rule_m,
            usize::from(j.rule_n),
            members,
            String::new(),
            "tor".to_string(),
            false,
        );
        self.session.active_workspace = id;
        // straight into the joined republic — no completion-screen stopover
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// One tick of the mock join: advance the percentage and append a
    /// phase-specific live-log line; the run flips to success at 100 %
    /// (every non-empty invite is accepted for now).
    fn join_tick(&mut self) {
        let j = &mut self.session.join;
        j.run.progress_pct = (j.run.progress_pct + 2).min(100);
        let t = u32::from(j.run.progress_pct / 2);
        if j.run.progress_pct >= 100 {
            j.run.log.push(format!(
                "✓ joined {} — {}-of-{} · workspace synced",
                j.republic, j.rule_m, j.rule_n
            ));
            j.run.outcome = 1;
            return;
        }
        if j.run.progress_pct < 30 {
            if t % 2 == 0 {
                let who = if j.inviter.is_empty() {
                    "the inviter"
                } else {
                    j.inviter.as_str()
                };
                j.run
                    .log
                    .push(format!("→ smp: contacting {who} · tor circuit hop {t}"));
            } else {
                j.run
                    .log
                    .push("→ mls: KeyPackage published · awaiting welcome".to_string());
            }
        } else if j.run.progress_pct < 75 {
            if t == 15 {
                j.run.log.push(format!(
                    "↓ mls: welcome received · epoch 0 · group {}",
                    j.republic
                ));
            } else {
                j.run
                    .log
                    .push(format!("↓ sync: surface batch {}/22 · sha256 ok", t - 15));
            }
        } else if t % 3 == 0 {
            j.run
                .log
                .push(format!("→ ws: {} materialized locally", j.republic));
        } else {
            j.run
                .log
                .push(format!("→ verify: merkle node {t} ok · member proof valid"));
        }
    }

    // ---- the shared self-ticker ------------------------------------------

    /// Drive a mock run from the engine itself (co-equal: the run makes
    /// progress no matter which operator started it, GUI attached or not).
    /// `tick` is re-sent every 90 ms; the task stops as soon as a tick is
    /// answered with an error (the run is over or was cancelled).
    pub(crate) fn spawn_ticker(&self, tick: Command) {
        let tx = self.cmd_tx.clone();
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
