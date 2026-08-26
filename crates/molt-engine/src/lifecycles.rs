// SPDX-License-Identifier: GPL-3.0-or-later

//! The engine-run lifecycles: **founding** (create) and **join** own the
//! command surface and wizard state; their network legs run off-actor over
//! Nostr since N4/N5 (`nostr_ritual.rs`: the founder's inbox + group tasks,
//! the member ladder's Nostr leg), and the loopback seams drive the same
//! rituals in tests. **restore** is real
//! (`backup_restore_design.md` §4/§6.6): an off-actor task fetches (file or
//! S3) and stages the encrypted blob, the ACTOR hard-verifies the
//! threshold-signed chain before anything materializes, and the restored
//! workspace opens *detached* (knowledge, not membership — rejoining is the
//! recovery ritual). They share a [`RunCore`] (step / progress / outcome /
//! log) and cancel-to-choice.

use molt_core::{
    demo_workspace_id, roster_members, Command, CreateState, JoinState, MemberId, MemberInfo,
    MoltError, Reply, RestoreState, RunCore, Screen, SessionScope, WorkspaceId, WorkspaceInfo,
};
use tokio::sync::oneshot;

use crate::{now_secs, ActiveStorage, Envelope, State};

/// The transport family + Nostr material a materializing workspace seals
/// into its v4 `transport.state`. Default = the legacy queue shape (no
/// discriminator) — loopback tests and (until N4b) recovery.
#[derive(Debug, Clone, Default)]
pub(crate) struct TransportShape {
    pub kind: Option<molt_core::TransportKind>,
    pub relays: Vec<String>,
    pub rotation_seed: Option<Vec<u8>>,
}

impl TransportShape {
    /// The Nostr shape of a sealed founding/join: discriminator + the group
    /// relay list + the h-tag seed.
    pub(crate) fn nostr(relays: Vec<String>, rotation_seed: [u8; 32]) -> Self {
        TransportShape {
            kind: Some(molt_core::TransportKind::Nostr),
            relays,
            rotation_seed: Some(rotation_seed.to_vec()),
        }
    }
}

/// Guard shared by every `*Start`: refuse while that run is in flight.
fn guard_idle(run: &RunCore, err: fn(String) -> MoltError) -> Result<(), MoltError> {
    if run.running() {
        return Err(err("already running".to_string()));
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
        // the ratified feature set (roster-v5); `None` on pre-v5 paths —
        // it must reach the genesis exactly as signed, like `agenda`
        features: Option<Vec<String>>,
        // which transport family this workspace runs on + its Nostr material
        // (relay list, rotation seed) — all persisted into the v4
        // `transport.state`. `TransportShape::default()` = the legacy
        // queue-shaped state (loopback tests, recovery until N4b).
        shape: TransportShape,
        // this node's ALREADY-derived identity signing key (the ritual anchors
        // it under a workspace-id-derived string, so re-deriving from the member
        // handle here would NOT match the roster — it must be passed in). `None`
        // for paths without a real founding (restore/demo), which have no chain.
        signing_key: Option<molt_storage::SigningKey>,
        // this node's ALREADY-derived nostr transport secret (the third
        // anchor's private half). Ticket-salted in the ritual, so it can never
        // be re-derived here — it must be passed in. `None` where the ticket
        // is genuinely gone (recovery — the old device held it; recovery-link
        // v2 owns that story) or on pre-ritual paths.
        nostr_sk: Option<Vec<u8>>,
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
        // captured before `shape` is consumed into the TransportState below
        let nostr_shape = shape.kind == Some(molt_core::TransportKind::Nostr);
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
            features,
            // the RATIFIED pool. `shape.relays` is the same list the founder
            // picked, the members signed and the genesis must carry — a
            // placeholder here writes a genesis whose own attestations do not
            // verify against it.
            relays: shape.relays.clone(),
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
        // VERIFY BEFORE THE FIRST DISK WRITE (review R1): a founding/join
        // genesis that does not verify must never become a persisted,
        // chainless workspace reported as sealed. A served (pruned) chain
        // was hard-verified by the recovery path already.
        if checkpoint_blob.is_none() && !chain.is_empty() {
            crate::chain::verify_chain(&chain)
                .map_err(|e| err(format!("the genesis does not verify - {e}")))?;
        }
        let mut opened = molt_storage::create_workspace(&root, &entropy, &genesis)
            .map_err(|e| err(e.to_string()))?;
        // seal the node's own MLS group state + assembled mesh into
        // transport.state **durably and synchronously**, before the writer task
        // takes over the file: the group was just born in the ritual and a
        // fire-and-forget save could drop it (queue full / crash) leaving a
        // workspace that can never decrypt. The dir is fresh, so a state carrying
        // the MLS blob + mesh is complete.
        // the same material the sealed state below gets, kept for the LIVE
        // actor: `transport.state` is what a reopen reads, but this session
        // must be able to speak as itself immediately (N4b §8.8 step 5a)
        let adopt_kind = shape.kind;
        let adopt_relays = shape.relays.clone();
        let adopt_seed = shape.rotation_seed.clone();
        let adopt_mls = mls_snapshot.clone();
        let adopt_nostr_sk: Option<zeroize::Zeroizing<Vec<u8>>> = if chain.is_empty() {
            None
        } else {
            nostr_sk.clone().map(zeroize::Zeroizing::new)
        };
        if mls_snapshot.is_some() || !mesh.is_empty() || !chain.is_empty() {
            let ts = molt_core::TransportState {
                mls: mls_snapshot,
                mesh,
                identity_sk: if chain.is_empty() { None } else { sk_bytes },
                nostr_sk: if chain.is_empty() { None } else { nostr_sk },
                kind: shape.kind,
                relays: shape.relays,
                rotation_seed: shape.rotation_seed,
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
            // a pruned chain under a v1 manifest would let an OLD binary
            // open this fresh workspace chainless — raise the gate now
            if checkpoint_blob.is_some() {
                opened.bump_pruned_version().map_err(|e| err(e.to_string()))?;
            }
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
            self.set_checkpoint_blob(checkpoint_blob);
            self.adopt_chain(chain);
            self.note_governance_readiness();
        }
        self.adopt_nostr_transport(
            adopt_kind,
            adopt_nostr_sk.as_ref().map(|sk| sk.as_slice()),
            &adopt_relays,
            adopt_seed.as_deref(),
        );
        let prefs = opened.prefs.clone();
        // NOTE the order: the runtime start below needs `self.active`, so it
        // cannot sit up with the other adoptions.
        self.active = Some(ActiveStorage {
            id: id.clone(),
            dir,
            prefs,
            handle: molt_storage::start_writer(opened),
        });
        // The FIRST session must be as honest as a reopen. `net_health` was
        // written on the open path only, so a freshly founded or joined Nostr
        // workspace kept the serde default (`Ok`) and showed a green pill for
        // its whole first session — promising a runtime that does not exist
        // until N5. Both callers emit_session(Full) after this returns.
        if nostr_shape {
            // bring the group runtime up NOW, not only on the next reopen: a
            // freshly founded republic that could not talk until it was closed
            // and opened again would be a first session less capable than a
            // resumed one — the F1 honesty finding in another costume. It runs
            // HERE because `build_group_net` needs `self.active`, which is set
            // just above.
            if self.nostr.is_some() {
                if let Some(blob) = adopt_mls.as_deref() {
                    self.group_net = self.build_group_net(blob);
                    self.spawn_seat_inbox_if_nostr();
                }
            }
            // the FIRST session is as honest as a reopen: green only when the
            // group runtime actually came up (N5.2), never on the serde default
            self.session.net_health = if self.group_net.is_some() {
                molt_core::NetHealth::Ok
            } else {
                molt_core::NetHealth::Down {
                    reason: crate::session::NOSTR_RUNTIME_PENDING.to_string(),
                }
            };
        }
        // B2: a fresh materialization seeds the seat's read cursors (a
        // founding/join has little history; a restore may carry plenty)
        self.adopt_read_cursors();
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
        // the finishes materialize the directory before pushing the entry,
        // so its real footprint is on disk right now; a session-only demo
        // entry has no directory and honestly reports 0
        let size_kib = self
            .active
            .as_ref()
            .filter(|a| a.id == *id)
            .map(|a| crate::session::entry_size_kib(&a.dir))
            .unwrap_or(0);
        self.session.workspaces.push(WorkspaceInfo {
            id: id.clone(),
            name: name.to_string(),
            detail: WorkspaceInfo::rule_detail(rule_m, rule_n),
            synced: true,
            state: 0,
            last_sync_min: 0,
            sync_queue: 0,
            s3,
            size_kib,
            // honest: nothing has been uploaded yet — the stamp moves only
            // on a confirmed upload (NetBackupDone), never on enable
            last_backup_min: WorkspaceInfo::NEVER,
            backup_copies: 0,
            backup_error: String::new(),
            seed,
            net,
            agenda,
            encrypted: false,
            members: members.clone(),
            restored: false,
        });
        // the stamps this entry starts with are real observations (the seal
        // round, the announces a recovery reached) - and presence knowledge
        // only survives a restart if it reaches the workspace's prefs.toml
        self.remember_seen(
            members
                .into_iter()
                .filter(|m| m.last_seen != MemberInfo::NEVER)
                .map(|m| (m.name, m.last_seen))
                .collect(),
        );
    }

    // ---- restore (real: design §4 / §6.6) ------------------------------

    /// Begin a real restore: validate synchronously (way, secret, and for
    /// the s3 way the SAVED backup target + fail-closed dialer), then run
    /// the fetch + decrypt + stage OFF the actor. The task reports real
    /// progress as [`Command::NetRestoreProgress`] and parks its staged
    /// result for [`State::cmd_net_restore_staged`], where the engine
    /// hard-verifies the chain BEFORE anything materializes.
    pub(crate) fn cmd_restore_start(
        &mut self,
        way: String,
        target: String,
        secret: String,
        replace: bool,
    ) -> Result<Reply, MoltError> {
        guard_idle(&self.session.restore.run, MoltError::Restore)?;
        if !self.persist {
            return Err(MoltError::Restore(
                "this node has no workspace storage to restore into".to_string(),
            ));
        }
        // the master secret (phrase / passphrase) rides in a wiped-on-drop
        // wrapper across the task hop, matching the export path's posture
        let secret = zeroize::Zeroizing::new(secret.trim().to_string());
        if secret.is_empty() {
            return Err(MoltError::Restore(
                "the restore needs its secret: the recovery phrase for S3/auto \
                 backups, the export passphrase for manual file exports"
                    .to_string(),
            ));
        }
        let target = target.trim().to_string();
        let planned = match way.as_str() {
            "file" => {
                if target.is_empty() {
                    return Err(MoltError::Restore(
                        "a file restore needs the .molt.enc path".to_string(),
                    ));
                }
                RestorePlan::File(molt_storage::expand_tilde(&target))
            }
            "s3" => {
                let s = &self.session.settings;
                let config = molt_net::s3::S3Config::from_settings(
                    &s.s3_endpoint,
                    &s.s3_access_key,
                    &s.s3_secret_key,
                    &s.s3_bucket,
                )
                .map_err(|e| MoltError::Restore(format!("backup target: {e}")))?;
                // fail-closed: a Tor misconfiguration aborts the restore
                let dialer = self.resolve_dialer().map_err(MoltError::Restore)?;
                let object = if molt_core::parse_backup_key(&target).is_some() {
                    S3Pick::Object(target.clone())
                } else if target.len() == 64
                    && target
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    S3Pick::NewestOf(target.clone())
                } else {
                    return Err(MoltError::Restore(
                        "pick a backup: pass the workspace id from the backup \
                         table (64 hex chars) or a full object key \
                         molt/<id>/<ts>.molt.enc"
                            .to_string(),
                    ));
                };
                RestorePlan::S3 {
                    config: Box::new(config),
                    dialer,
                    pick: object,
                }
            }
            other => {
                return Err(MoltError::Restore(format!(
                    "unknown restore way `{other}` (s3 | file; rejoining is recover_start)"
                )));
            }
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return Err(MoltError::Restore("engine stopped".to_string()));
        };
        // a restarted restore supersedes the one in flight
        self.restore_generation += 1;
        let generation = self.restore_generation;
        self.restore_staging = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = self.restore_staging.clone();
        self.restore_replace = replace;
        self.restored_id = None;
        let root = self.workspace_root();
        let mut run = RunCore::started();
        run.log.push(format!("→ restore started · way {way} · {target}"));
        self.session.restore = RestoreState { run, way, target };
        self.session.screen = Screen::Restore;
        self.emit_session(SessionScope::Full);
        let task =
            tokio::spawn(
                async move { restore_task(cmd_tx, generation, planned, root, secret, slot).await },
            );
        self.restore_task = Some(task);
        Ok(Reply::Ack)
    }

    /// Real progress from the off-actor restore task (engine-internal).
    pub(crate) fn cmd_net_restore_progress(
        &mut self,
        pct: u8,
        line: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.restore_generation) || !self.session.restore.run.running() {
            return Ok(Reply::Ack);
        }
        // 100 is reserved for the verified finish; the bar never regresses
        let r = &mut self.session.restore.run;
        r.progress_pct = r.progress_pct.max(pct.min(99));
        r.log.push(line);
        self.emit_session(SessionScope::Restore);
        Ok(Reply::Ack)
    }

    /// The staged blob arrived (engine-internal): run the MANDATORY chain
    /// verification and the genesis/manifest consistency checks on the
    /// actor — hard-reject, all-or-nothing (`persistent_chain.md`) — and
    /// only then commit the staging into the workspace root. The staged
    /// handle rides an engine-internal slot, so a forged command without a
    /// real staged blob is a no-op failure, never a materialization.
    pub(crate) fn cmd_net_restore_staged(
        &mut self,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.restore_generation) || !self.session.restore.run.running() {
            return Ok(Reply::Ack);
        }
        let staged = self.restore_staging.lock().ok().and_then(|mut s| s.take());
        let Some(staging) = staged else {
            return self.fail_restore("the restore task lost its staged blob".to_string());
        };

        // ---- verify (design §4.1 step 2; hard-reject) ----
        if staging.chain.is_empty() {
            staging.abort();
            return self.fail_restore(
                "the backup carries no verifiable chain - refused".to_string(),
            );
        }
        // no external anchor exists for an import — the content-derived
        // republic id the genesis/blob founding table recomputes IS the
        // trust anchor (the full-chain forgery check), same helper the
        // recovery adoption uses with its link anchor
        let (head, sealed) =
            match crate::chain::verify_served(staging.checkpoint.as_ref(), &staging.chain, None) {
                Ok(pair) => pair,
                Err(e) => {
                    staging.abort();
                    return self.fail_restore(format!("chain verification failed: {e}"));
                }
            };
        // the manifest is the unauthenticated cover sheet — the verified
        // genesis is authoritative (name is a display value the republic
        // may have legitimately renamed; the RULE must agree)
        if staging.manifest.workspace.rule_m != sealed.rule_m
            || staging.manifest.workspace.rule_n != sealed.rule_n
        {
            staging.abort();
            return self.fail_restore(
                "the manifest contradicts the verified genesis (threshold rule)".to_string(),
            );
        }
        // the log's Founded genesis and the chain must describe the SAME
        // republic, and this workspace's member must hold a roster seat
        let molt_core::WorkspaceEvent::Founded {
            member: ws_member,
            republic_id: log_rid,
            ..
        } = staging.genesis.body.clone()
        else {
            staging.abort();
            return self.fail_restore("the log genesis is not a Founded event".to_string());
        };
        if log_rid != sealed.republic_id || head.republic_id != sealed.republic_id {
            staging.abort();
            return self.fail_restore(
                "the log genesis and the verified chain disagree on the republic".to_string(),
            );
        }
        if !sealed.roster.contains(&ws_member) {
            staging.abort();
            return self.fail_restore(
                "this workspace's member holds no seat in the verified roster".to_string(),
            );
        }

        // the seat identity, when the seed travels: derive BOTH ritual
        // derivations (founder salts with the workspace id, a joiner with
        // the shared member salt) and accept only the one the VERIFIED
        // head anchors — never an unanchored guess (CLAUDE.md: re-deriving
        // with the wrong salt gives the wrong key silently)
        let ws_id = staging.manifest.workspace.id.clone();
        let identity_sk = staging.seed_entropy().and_then(|seed| {
            let anchored = head.identities.iter().find(|i| i.member == ws_member)?;
            // joiner salt: the ONE shared derivation of founding.rs
            let (sk, pk) = crate::founding::member_identity_from_entropy(seed);
            if pk == anchored.identity_pk {
                return Some(sk);
            }
            // founder salt: the founder's own workspace id (the ritual's
            // start_ritual derivation — which IS the manifest id here)
            let (sk, pk) = molt_storage::derive_identity_key(seed, &ws_id);
            (pk == anchored.identity_pk).then_some(sk)
        });
        let seed_present = staging.seed_entropy().is_some();

        // never materialize over LIVE state: the collision/replace path
        // (design §4.3) is for CLOSED dirs. Committing over the OPEN
        // workspace would `fs::rename` the active directory into `.trash`
        // from under its own running writer (ENOENT writes, a dangling
        // `self.active`, a duplicate id dir); a backup in flight is reading
        // the very files a replace moves. Refuse honestly in both cases —
        // the staged blob is dropped, nothing is touched on disk.
        if self.session.active_workspace == ws_id {
            staging.abort();
            return self.fail_restore(format!(
                "workspace {ws_id} is currently open - close it before restoring over it"
            ));
        }
        if self.backup_inflight.contains(&ws_id) {
            staging.abort();
            return self.fail_restore(format!(
                "a backup of workspace {ws_id} is in flight - retry once it completes"
            ));
        }

        // ---- commit (design §4.1 step 3) ----
        let created = staging.created;
        let at_rest = staging.at_rest.clone();
        let name = staging.manifest.workspace.name.clone();
        let root = self.workspace_root();
        let dir = match staging.commit(&root, self.restore_replace, identity_sk.as_ref()) {
            Ok(dir) => dir,
            Err(molt_storage::StorageError::Exists(_)) => {
                return self.fail_restore(
                    "a workspace with this id already exists - delete it, or restore \
                     with replace enabled"
                        .to_string(),
                );
            }
            Err(e) => return self.fail_restore(format!("materializing failed: {e}")),
        };

        // ---- the honest finish: knowledge restored, membership not ----
        let members = roster_members(&sealed.roster, self.presence_now(), |_| MemberInfo::NEVER);
        let entry_seed = molt_storage::read_sealed_seed(&root, &dir, &ws_id).unwrap_or_default();
        let size_kib =
            u32::try_from(molt_storage::workspace_size_kib(&dir)).unwrap_or(u32::MAX);
        let encrypted = at_rest == molt_core::SEALED_PHRASE;
        // node-local prefs travel in the blob (§3.2) — the entry mirrors the
        // RESTORED prefs (same source the boot scan reads), or the list and
        // the next restart would disagree about the auto-backup toggle
        let restored_prefs = molt_storage::read_prefs(&dir);
        // a replace committed NEW content under an existing id — the old
        // session row (name/size/encrypted/…) no longer describes it
        self.session.workspaces.retain(|w| w.id != ws_id);
        {
            self.session.workspaces.push(WorkspaceInfo {
                id: ws_id.clone(),
                name: name.clone(),
                detail: WorkspaceInfo::rule_detail(sealed.rule_m, usize::from(sealed.rule_n)),
                // honest §4.4 state: a detached workspace has no mesh and
                // cannot sync — it is offline, not "synced just now"
                synced: false,
                state: 2,
                last_sync_min: 0,
                sync_queue: 0,
                s3: restored_prefs.s3_backup,
                size_kib,
                // the restored prefs' own stamp (exactly what the boot scan
                // would show after a restart), aged to now; NEVER when the
                // blob never carried one
                last_backup_min: restored_prefs
                    .last_backup
                    .map(|ts| {
                        u32::try_from(crate::now_secs().saturating_sub(ts) / 60)
                            .unwrap_or(u32::MAX - 1)
                    })
                    .unwrap_or(WorkspaceInfo::NEVER),
                backup_copies: 0,
                backup_error: String::new(),
                seed: entry_seed,
                net: self.effective_net_label(),
                encrypted,
                restored: false,
                members,
                agenda: sealed.agenda.clone(),
            });
        }
        self.restored_id = Some(ws_id.clone());
        let age_days = crate::now_secs().saturating_sub(created) / 86_400;
        let r = &mut self.session.restore.run;
        r.progress_pct = 100;
        r.outcome = 1;
        r.log.push(format!(
            "✓ chain verified · height {} · {}-of-{}",
            head.height, sealed.rule_m, sealed.rule_n
        ));
        r.log.push(format!(
            "✓ backup from unix {created} ({age_days} day(s) old) · workspace “{name}” materialized"
        ));
        if seed_present && identity_sk.is_none() {
            r.log.push(
                "→ the seed does not anchor this seat in the verified roster - \
                 knowledge-only restore"
                    .to_string(),
            );
        }
        r.log.push(
            "→ knowledge restored - the workspace opens detached and reattaches \
             automatically"
                .to_string(),
        );
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The restore task failed (engine-internal): surface the reason
    /// verbatim in the run log.
    pub(crate) fn cmd_net_restore_failed(
        &mut self,
        error: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.restore_generation) || !self.session.restore.run.running() {
            return Ok(Reply::Ack);
        }
        self.fail_restore(error)
    }

    /// Shared failure tail: flip the run to failed with the honest reason
    /// and drop any staged blob (removing its staging dir).
    fn fail_restore(&mut self, error: String) -> Result<Reply, MoltError> {
        if let Ok(mut slot) = self.restore_staging.lock() {
            slot.take(); // drop removes the staging dir
        }
        tracing::warn!(error = %error, "restore failed");
        let headline = crate::relay_msg::restore_headline_for(&error);
        let r = &mut self.session.restore.run;
        r.outcome = 2;
        r.log.push(format!("✗ restore failed: {error}"));
        // the few words the operator READS; the sentence above keeps the
        // detail. Unrecognised failures leave it empty on purpose — the
        // surface then shows its generic failed-title rather than a guess.
        r.headline = headline;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_restore_cancel(&mut self) -> Result<Reply, MoltError> {
        // invalidate the in-flight task (its late results are dropped by the
        // generation guard) and abort it — the download is inbound-only, so
        // abort is safe (nothing outbound is in flight)
        self.restore_generation += 1;
        if let Some(task) = self.restore_task.take() {
            task.abort();
        }
        if let Ok(mut slot) = self.restore_staging.lock() {
            slot.take(); // drop removes the staging dir
        }
        // a blocking stage that outlives the abort parks into THIS Arc;
        // replacing it makes the task's clone the last owner, so the staged
        // dir is swept the moment the task ends
        self.restore_staging = std::sync::Arc::new(std::sync::Mutex::new(None));
        self.restored_id = None;
        self.session.restore = RestoreState::default();
        self.session.screen = Screen::Choice;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_restore_finish(&mut self) -> Result<Reply, MoltError> {
        guard_finished(&self.session.restore.run, MoltError::Restore)?;
        let Some(id) = self.restored_id.clone() else {
            return Err(MoltError::Restore(
                "no restored workspace to open".to_string(),
            ));
        };
        // a phrase-sealed blob round-tripped SEALED (S6): there is no key
        // material to open with — land on the workspace list, where the
        // existing decrypt flow takes over, instead of dead-ending on the
        // open refusal
        if self
            .session
            .workspaces
            .iter()
            .any(|w| w.id == id && w.encrypted)
        {
            self.restored_id = None;
            self.session.restore = RestoreState::default();
            self.session.screen = Screen::Open;
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }
        // opens DETACHED: the imported dir carries no mesh credentials and
        // no MLS state on purpose (§3.3/§4.4) — cmd_open_workspace comes up
        // without a mesh and sets the honest detached notice
        self.cmd_open_workspace(id)?;
        self.restored_id = None;
        self.session.restore = RestoreState::default();
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- founding (create): the ritual (transport concept §3.3) --------

    pub(crate) fn cmd_create_start(
        &mut self,
        name: String,
        member: String,
        threshold: u8,
        members: u8,
        // the founder's deliberate relay pick (empty = whatever this node can
        // dial). It becomes the invites' list, the Welcome's, and — with R3 —
        // the pool every member signs into the genesis.
        relays: Vec<String>,
    ) -> Result<Reply, MoltError> {
        guard_idle(&self.session.create.run, MoltError::Create)?;
        let name = name.trim().to_string();
        let member = member.trim().to_string();
        if name.is_empty() {
            return Err(MoltError::Create("the name must not be empty".to_string()));
        }
        crate::founding::check_handle(&member).map_err(MoltError::Create)?;
        // threshold 1 is refused since 2026-08-08 (product decision): a lone
        // voice is no threshold. The gate sits HERE, not in the verifier —
        // the genesis is immutable, and rejecting existing m=1 chains at
        // adoption would brick them into the silent-legacy trap.
        if threshold < 2 || threshold > members || !(2..=13).contains(&members) {
            return Err(MoltError::Create(
                "threshold must be within 2..=members and members within 2..=13".to_string(),
            ));
        }
        // pool-settled gate, BEFORE the destructive prelude below: a probe
        // still in flight means the pool is about to change — minting
        // invites now would silently drop the relay the operator just
        // consented to. Refusing any later would first have destroyed the
        // operator's in-flight join/recovery and told every peer, for a
        // transient condition that invites a retry (review 2026-08-16).
        // Keystone in tests/relay_pool.rs.
        if !self.ritual_sim && !self.pending_relay_confirms.is_empty() {
            return Err(MoltError::Create(format!(
                "cannot found: {}",
                crate::relay_msg::pool_verifying_reason()
            )));
        }
        // The founder's recovery phrase is real entropy — the workspace id
        // and every key hangs off it. It is shown once during the ritual
        // and never persisted into the shared session of a real workspace.
        let seed =
            molt_storage::generate_seed_phrase().map_err(|e| MoltError::Create(e.to_string()))?;

        // any prior ritual/mesh belongs to a different context — and so does
        // any in-flight JOIN: without this its late seal materializes a
        // foreign republic mid-founding (see invalidate_join). Anyone waiting
        // on the abandoned ritual is told rather than left hanging.
        self.abandon_ritual("the founder started a new founding");
        self.ritual_attestations.clear();
        self.invalidate_join();
        self.invalidate_recovery();
        let crate::founding::RitualStart { links, notes } = self
            .start_ritual(&name, &member, threshold, members, &seed, &relays)
            .map_err(MoltError::Create)?;

        // the founder shares the invite links off-band and waits for real
        // members to join. Only the offline sim test seam simulates the
        // other members.
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
            features: Vec::new(),
            can_propose: false,
            backup_confirmed: false,
            member: member.clone(),
            threshold,
            members,
            net: self.effective_net_label(),
            seed,
            seats,
            simulated,
        };
        self.session.create.run.log.push(format!(
            "→ ritual opened · {member} (founder) · {threshold}-of-{members} · {} invite(s) minted",
            usize::from(members).saturating_sub(1)
        ));
        // …then whatever start_ritual needed to tell the operator (the pool
        // was capped to what a link may carry, …) — pushed HERE because the
        // assignment above replaced the whole CreateState
        self.session.create.run.log.extend(notes);
        if simulated {
            self.session.create.run.log.push(
                "→ SIMULATION - no real network in this build: this node signs for \
                 every member"
                    .to_string(),
            );
        } else {
            self.session.create.run.log.push(
                "→ share each link off-band over a private channel - the ritual waits \
                 for the activations"
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
        // simply stays listed, just not entered). The members are TOLD —
        // otherwise they sit in an unbounded wait forever.
        self.abandon_ritual("the founder cancelled it");
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
        // every seat must be sealed AND backup-confirmed (state 4), and the
        // founder's own phrase must be re-typed too — the ritual's FIRST
        // disk write waits for the last confirmation
        // (seed_backup_confirmation.md ❻½, n-of-n incl. the founder)
        if self.session.create.seats.iter().any(|s| s.state != 4) {
            return;
        }
        if !self.session.create.backup_confirmed {
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
                // sealed ≠ entered (2026-08-08): the wizard's last step makes
                // the founder back its phrase up first — `CreateFinish` enters,
                // exactly like the joiner's `JoinFinish`. The post-founding
                // mesh comes up in the background; the `create` state is kept
                // (not reset) so the wizard's final log lines (incl. "direct
                // mesh established") still land.
            }
            Err(e) => {
                self.session.create.run.outcome = 2;
                self.session.create.run.headline = crate::relay_msg::headline_for(&e.to_string());
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
        // the group's relay pool goes into the signed bytes (R3): the founder
        // PICKED it (R2c) and every member ratifies it here, so from the
        // genesis on it is group state nobody can change alone
        // from the RITUAL PARAMETER, not `self.net_ritual`: `maybe_finalize`
        // took it before calling here, so reading the field would silently
        // seal an empty pool — which the R3 keystone caught.
        let pool: Vec<String> = ritual.group_relays();
        // the ratified feature set travels the same way as the pool: from
        // the ritual parameter, exactly as the members signed it
        let features = ritual.features();
        let table = molt_core::roster_canonical_bytes(
            &republic_id,
            c.threshold,
            c.members,
            &identities,
            &c.agenda,
            &pool,
            features.as_deref(),
        );
        let founder_sig = molt_storage::identity_sign(ritual.founder_sk(), &table);
        let mut attestations = vec![molt_core::RosterAttestation {
            member: c.member.clone(),
            sig: founder_sig,
        }];
        attestations.append(&mut self.ritual_attestations.clone());

        // the complete sealed roster every member (founder included) writes
        let sealed = molt_core::SealedRoster {
            relays: pool.clone(),
            name: c.name.clone(),
            republic_id: republic_id.clone(),
            rule_m: c.threshold,
            rule_n: c.members,
            roster: roster.clone(),
            identities: identities.clone(),
            attestations: attestations.clone(),
            agenda: c.agenda.clone(),
            features: features.clone(),
        };
        // what every member writes must verify HERE too (review R2): an
        // attestation collected over a pre-charter table would otherwise be
        // persisted, `adopt_chain` would reject the genesis, and the wizard
        // would still report "sealed by everyone"
        crate::founding::verify_sealed_roster(&sealed)
            .map_err(|e| MoltError::Create(format!("the sealed roster does not verify - {e}")))?;

        // the founder's MLS group. On the Nostr path it was BORN at
        // all-joined (the Welcomes are already gift-wrapped out — concept
        // §4.2: deliberation ran as 445s inside it), so finalize ENCRYPTS
        // the genesis frame first and only then snapshots the live group:
        // the encrypt advances the sender ratchet, and a snapshot taken
        // before it would persist a state that re-uses the genesis
        // generation on its next message — a SecretReuseError on every
        // member after reopen. Rebuilding the group here would be worse
        // still (a second group nobody was welcomed into). On the loopback
        // path it is built now, from every seat's KeyPackage, BEFORE
        // touching disk, so a missing/invalid package fails the founding
        // cleanly (only for a persisted founding — the demo has no
        // workspace to hold a group, and its sim members ignore the
        // Welcome anyway).
        let mut nostr_genesis_frame: Option<(Vec<u8>, [u8; 32])> = None;
        let (founder_mls_member, welcome, founder_mls_blob) =
            if let Some(group) = ritual.nostr_group() {
                let sealed_json = serde_json::to_string(&sealed)
                    .map_err(|e| MoltError::Create(e.to_string()))?;
                let genesis_payload =
                    serde_json::to_vec(&molt_net::invite::RitualMsg::Genesis {
                        sealed: sealed_json,
                        // the Welcome went out at group birth — never here
                        welcome: String::new(),
                    })
                    .map_err(|e| MoltError::Create(e.to_string()))?;
                let mut g = group
                    .lock()
                    .map_err(|_| MoltError::Create("mls lock poisoned".to_string()))?;
                let ct = g
                    .encrypt(&genesis_payload)
                    .map_err(|e| MoltError::Create(e.to_string()))?;
                let exporter = g
                    .exporter_secret()
                    .map_err(|e| MoltError::Create(e.to_string()))?;
                let blob = g.snapshot().map_err(|e| MoltError::Create(e.to_string()))?;
                drop(g);
                nostr_genesis_frame = Some((ct, exporter));
                (None, String::new(), Some(blob))
            } else if self.persist {
                let (mls, welcome) = ritual.build_founder_mls().map_err(MoltError::Create)?;
                let blob = mls.snapshot().map_err(|e| MoltError::Create(e.to_string()))?;
                // the live group is kept (not just the blob) only where the
                // mesh bootstrap needs to drive post-founding announcements
                (Some(mls), welcome, Some(blob))
            } else {
                (None, String::new(), None)
            };

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
                features.clone(),
                ritual.transport_shape(),
                // the founder's identity key, exactly as anchored in the roster
                Some(ritual.founder_sk().clone()),
                // …and its nostr transport secret (self-ticket-salted in
                // start_ritual; the ticket is gone, only the key survives)
                Some(ritual.founder_nostr_sk().to_vec()),
                founder_mls_blob,
                // the founder's mesh is not known until its bootstrap finishes;
                // it is persisted then, via NetMeshReady
                Vec::new(),
                None, // a founding's chain IS the genesis
                None, // …rooted, never pruned at birth
                MoltError::Create,
            )?;
            // truthful marker: only a sim-seam founding has simulated
            // members. A REAL founding must never carry the flag — marked
            // simulated, the demo-mesh seam would grow fake peers over a
            // real republic's log.
            self.persist_simulated_members(&id, self.ritual_sim);
            id
        } else {
            demo_workspace_id(&c.name)
        };

        // only now distribute the sealed roster so each member writes its
        // own workspace (own seed) and enters from the same constitution.
        //
        // Nostr: the frame was pre-encrypted above (ratchet coherence with the
        // snapshot) — the task only PUBLISHES it, and retries the publish
        // without ever re-encrypting.
        //
        // This leg is the members' only path into the republic, and it runs
        // AFTER the founder has materialized, so it cannot use the ritual's
        // failure sink: `maybe_finalize` already `take()`n the ritual, so a
        // generation-gated report would be dropped, and `cmd_net_ritual_failed`
        // early-returns once the run has an outcome. It therefore reports with
        // `generation: None` and gets its own surface.
        //
        // (The old comment here claimed "the member's own open wait surfaces a
        // relays-down condition". It does not: that wait is unbounded.)
        let ws_id = id.clone();
        if let Some((ct, exporter)) = nostr_genesis_frame {
            if let (Some(chan), Some(tx)) = (ritual.nostr_chan(), self.cmd_tx.upgrade()) {
                crate::nostr_ritual::spawn_publish_frame(
                    chan,
                    crate::nostr_ritual::FramePayload::Sealed { ct, exporter },
                    "genesis",
                    crate::nostr_ritual::RetryPolicy::GENESIS,
                    tx.downgrade(),
                    None,
                    // the genesis outlives the ritual, so it carries the
                    // workspace it belongs to instead
                    ws_id.clone(),
                );
            }
        } else if let Ok(json) = serde_json::to_string(&sealed) {
            // loopback: the sealed roster + the MLS Welcome per reply queue
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
        // the ritual just sealed with every member's live participation —
        // that IS a real sighting, so all stamps start at now
        let now = self.presence_now();
        let members = roster_members(&roster, now, |_| now);
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

    /// The operator's phrase-backup confirmation during a RUNNING ritual
    /// (`seed_backup_confirmation.md` ❻½) — founder or joiner, co-equal
    /// (MCP tool + GUI). The engine matches the re-typed phrase; a
    /// mismatch is refused with an honest error, never silently accepted.
    pub(crate) fn cmd_confirm_seed_backup(&mut self, phrase: &str) -> Result<Reply, MoltError> {
        fn matches(typed: &str, seed: &str) -> bool {
            !seed.is_empty() && typed.split_whitespace().eq(seed.split_whitespace())
        }
        // founder side: an open founding awaiting its own confirmation
        if self.net_ritual.is_some() && self.session.create.run.outcome == 0 {
            if self.session.create.backup_confirmed {
                return Ok(Reply::Ack); // idempotent
            }
            if !matches(phrase, &self.session.create.seed) {
                return Err(MoltError::Create(
                    "the re-typed phrase does not match".to_string(),
                ));
            }
            self.session.create.backup_confirmed = true;
            self.session
                .create
                .run
                .log
                .push("✓ recovery phrase backed up".to_string());
            self.maybe_finalize();
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }
        // joiner side: paused awaiting the backup confirmation
        if self.session.join.awaiting_backup {
            if !matches(phrase, &self.session.join.seed) {
                return Err(MoltError::Join(
                    "the re-typed phrase does not match".to_string(),
                ));
            }
            if let Some(tx) = self.join_backup.take() {
                // the paused ritual task waits on this; closed = it moved on
                let _ = tx.try_send(true);
            }
            self.session.join.awaiting_backup = false;
            self.session.join.run.progress_pct = 92;
            self.session
                .join
                .run
                .log
                .push("✓ recovery phrase backed up · waiting for the others".to_string());
            self.emit_session(SessionScope::Full);
            return Ok(Reply::Ack);
        }
        Err(MoltError::Create(
            "no ritual awaits a backup confirmation".to_string(),
        ))
    }

    /// Persist the operator's LOCAL wiki draft for the open workspace
    /// (`shared_memory_real.md` WP-D): an opaque frontend blob beside the
    /// prefs — sealed at rest with the directory, excluded from the
    /// backup export by its allowlist. No open workspace = a quiet Ack
    /// (the debounced auto-save races closes; nothing to persist is not
    /// an error).
    pub(crate) fn cmd_wiki_draft_save(&mut self, draft: &str) -> Result<Reply, MoltError> {
        let id = self.session.active_workspace.clone();
        if id.is_empty() {
            return Ok(Reply::Ack);
        }
        let Some(dir) = molt_storage::find_workspace_dir(&self.workspace_root(), &id) else {
            return Ok(Reply::Ack); // session-only workspace: nothing to hold it
        };
        if let Err(e) = molt_storage::write_wiki_draft(&dir, draft) {
            tracing::warn!(error = %e, "wiki draft not persisted");
        }
        Ok(Reply::Ack)
    }

    /// Read the open workspace's stored wiki draft ("" = none).
    pub(crate) fn cmd_wiki_draft_load(&mut self) -> Result<Reply, MoltError> {
        let id = self.session.active_workspace.clone();
        let draft = if id.is_empty() {
            String::new()
        } else {
            molt_storage::find_workspace_dir(&self.workspace_root(), &id)
                .map(|dir| molt_storage::read_wiki_draft(&dir))
                .unwrap_or_default()
        };
        Ok(Reply::WikiDraft { draft })
    }

    pub(crate) fn cmd_create_finish(&mut self) -> Result<Reply, MoltError> {
        // "Enter republic" is refused until the ritual sealed a workspace
        // — the engine enforces it for every operator, not just the GUI
        if self.session.create.run.outcome != 1 {
            return Err(MoltError::Create(
                "the founding ritual is not complete - every member must sign first".to_string(),
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
        crate::founding::check_handle(&member).map_err(MoltError::Join)?;
        // the founding's twin gate: "adopt relays" confirms async, and a
        // join in the same breath would race the verdict — the link-carried
        // relays would be judged against a pool about to change
        if !self.pending_relay_confirms.is_empty() {
            return Err(MoltError::Join(
                crate::relay_msg::pool_verifying_reason().to_string(),
            ));
        }
        let invite = invite.trim().to_string();
        // a real join needs a link that carries the v2 transport handover —
        // a bare preview link (or a pre-N4 queue-shaped one) is not joinable,
        // and the parse error says which it was
        let inv = crate::founding::FoundingInvite::parse(&invite).map_err(MoltError::Join)?;
        // starting a join abandons any founding the user had open — its
        // recv loops must not seal and hijack the session behind our back,
        // and whoever was waiting on it is told. A recovery in flight goes
        // the same way: `cmd_recover_start` has always invalidated the join,
        // and the reverse direction only became load-bearing when a recovery
        // grew a task holding relay subscriptions (6e).
        self.abandon_ritual("the founder started a join instead");
        self.invalidate_recovery();
        // the joiner's own recovery phrase (shown once during the join); its
        // identity and its own workspace derive from it
        let seed =
            molt_storage::generate_seed_phrase().map_err(|e| MoltError::Join(e.to_string()))?;
        self.join_generation += 1;
        let generation = self.join_generation;
        self.session.join = JoinState {
            run: RunCore::started(),
            invite,
            member,
            awaiting_backup: false,
            republic: inv.info.republic.clone(),
            rule_m: inv.info.threshold,
            rule_n: inv.info.members,
            inviter: inv.info.inviter.clone(),
            seed,
            proposed_name: String::new(),
            proposed_agenda: String::new(),
            proposed_features: None,
            awaiting_ratify: false,
            sealed_id: String::new(),
        };
        self.session.screen = Screen::Join;
        // a fresh transport slot for this join incarnation (the loopback
        // seams still fill it; the Nostr join has no queue transport to park)
        self.join_transport = std::sync::Arc::new(std::sync::Mutex::new(None));
        // the Nostr join (N4a): resolve the fail-closed dialer, gate every
        // link-carried relay through the operator's OWN pool (ADR-0004 — a
        // pasted link must never make this node dial somewhere it has not
        // confirmed), then spawn the off-actor member task. Its results ride
        // the long-dormant NetJoin* commands.
        let dialer = match self.dialer_for() {
            Ok(d) => d,
            Err(e) => {
                return self.cmd_net_join_failed(format!("transport: {e}"), Some(generation))
            }
        };
        // The join runs over the relays BOTH sides can use: the invite names
        // the group's set, this node dials only what its own operator
        // confirmed (ADR-0004 — a pasted link never makes us dial somewhere
        // unapproved). **One relay in common is enough**; demanding the whole
        // set would refuse every join whose pools merely OVERLAP, which is the
        // normal case (the founder runs an onion relay, the invitee a clearnet
        // one, …). The group's full list is still what gets persisted — only
        // the dialing is narrowed.
        //
        // ONE judgement per relay, used for both the dial set and the refusal:
        // two independent readings of "can this node dial this one" could
        // disagree, and a refusal whose own detail lines contradict it is
        // worse than the flat message it replaced.
        let verdicts = molt_core::relay::diagnose_invite_relays(
            &inv.handover.relays,
            &self.session.settings.relays,
            self.clearnet_session,
        );
        let dial_relays: Vec<String> = verdicts
            .iter()
            .filter(|v| v.blocked.is_none())
            .map(|v| v.url.clone())
            .collect();
        if dial_relays.is_empty() {
            // Diagnose EVERY relay the invite names, individually. A flat "no
            // relay in common" was actively misleading whenever the relay was
            // in the operator's own pool but not dialable — the 2026-08-01
            // report ("config3 joined, config2 did not") was a hand-written
            // `confirmed = true` without `clearnet_enabled = true`, told it
            // had "no confirmed relay on this node".
            let refusal = crate::relay_msg::join_relay_refusal(
                &verdicts,
                &self.session.settings.relays,
                self.clearnet_session,
            );
            // one log line per relay (the log is rendered line by line, and
            // a wall of text is not read), then the terminal ✗ summary
            self.session.join.run.log.extend(refusal.detail);
            return self.cmd_net_join_failed(refusal.headline, Some(generation));
        }
        // the human ratification gate: the task blocks on this channel until
        // cmd_join_confirm_charter / cmd_join_decline_charter releases it
        let (confirm_tx, confirm_rx) = tokio::sync::mpsc::channel(1);
        self.join_confirm = Some(confirm_tx);
        // …and the phrase-backup gate right after it (❻½)
        let (backup_tx, backup_rx) = tokio::sync::mpsc::channel(1);
        self.join_backup = Some(backup_tx);
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return self.cmd_net_join_failed("engine stopped".to_string(), Some(generation));
        };
        // a restarted join aborts the previous incarnation's task (its
        // generation-guarded commands would be dropped anyway; aborting also
        // releases its sockets)
        if let Some(task) = self.join_task.take() {
            task.abort();
        }
        self.join_task = Some(crate::nostr_ritual::spawn_member_join(
            dialer,
            crate::nostr_ritual::JoinCtx {
                invite: inv,
                dial_relays,
                member: self.session.join.member.clone(),
                phrase: self.session.join.seed.clone().into(),
                generation,
            },
            confirm_rx,
            backup_rx,
            cmd_tx.downgrade(),
        ));
        Ok(Reply::Ack)
    }

    /// A real network join completed (dormant until N4's Nostr join re-emits
    /// it): verify what came from the off-actor task; write the joiner's own
    /// workspace from its own seed and enter the republic.
    #[allow(clippy::too_many_arguments)] // mirrors the Command's own field set
    pub(crate) fn cmd_net_join_sealed(
        &mut self,
        sealed: String,
        mls: String,
        mesh: Vec<molt_core::MeshLink>,
        nostr_sk: String,
        relays: Vec<String>,
        rotation_seed: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        // a cancelled/restarted join bumped the generation — drop stale results
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        // the Nostr shape rides the sealed report (relay list + rotation seed
        // from the authenticated Welcome). Both present = a Nostr join; both
        // empty = the loopback/test path. A half-present or malformed pair is
        // a broken task — fail rather than seal a workspace whose transport
        // can never come up.
        let shape = match (relays.is_empty(), rotation_seed.is_empty()) {
            (true, true) => TransportShape::default(),
            (false, false) => match hex::decode(&rotation_seed) {
                Ok(seed) if seed.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&seed);
                    TransportShape::nostr(relays.clone(), arr)
                }
                _ => {
                    return self.cmd_net_join_failed(
                        "the join task delivered a malformed rotation seed".to_string(),
                        generation,
                    )
                }
            },
            _ => {
                return self.cmd_net_join_failed(
                    "the join task delivered an incomplete Nostr transport shape".to_string(),
                    generation,
                )
            }
        };
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
            // the joiner's nostr transport secret comes FROM the ritual (the
            // ticket that salted it died with the join task) and pairs with
            // the seat's FOREVER-anchored nostr_pk — so it is validated like
            // the MLS blob above, never trusted: 32 bytes of hex whose
            // x-only public key equals OUR anchored anchor, or the join
            // FAILS. Persisting a mismatched/absent secret would seal a
            // genesis whose transport key this node cannot use — a defect
            // that surfaces only when N4's transport first needs the key,
            // with no re-derivation path.
            let joiner_nostr_sk = {
                let Some(our_seat) = sealed.identities.iter().find(|i| i.member == j.member)
                else {
                    return self.cmd_net_join_failed(
                        "the sealed roster does not anchor our seat".to_string(),
                        generation,
                    );
                };
                let mut raw = match hex::decode(&nostr_sk) {
                    Ok(raw) if raw.len() == 32 => raw,
                    _ => {
                        return self.cmd_net_join_failed(
                            "the join task delivered a malformed nostr transport secret"
                                .to_string(),
                            generation,
                        )
                    }
                };
                match molt_net::nostr_pk_for_sk(&raw) {
                    Ok(pk) if pk == our_seat.nostr_pk => Some(raw),
                    Ok(_) => {
                        zeroize::Zeroize::zeroize(&mut raw);
                        return self.cmd_net_join_failed(
                            "the delivered nostr transport secret is not the private half \
                             of our anchored transport key"
                                .to_string(),
                            generation,
                        );
                    }
                    Err(e) => {
                        zeroize::Zeroize::zeroize(&mut raw);
                        return self.cmd_net_join_failed(
                            format!("invalid nostr transport secret: {e}"),
                            generation,
                        );
                    }
                }
            };
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
                sealed.features.clone(),
                shape,
                joiner_sk,
                joiner_nostr_sk,
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
        self.join_backup = None;
        // every roster member just took part in the join ritual's seal —
        // a real sighting for each of them
        let now = self.presence_now();
        let members = roster_members(&sealed.roster, now, |_| now);
        self.push_workspace_entry(
            &id,
            &sealed.name,
            sealed.rule_m,
            sealed.roster.len(),
            members,
            j.seed.clone(),
            self.effective_net_label(),
            self.session.settings.s3_backup,
            sealed.agenda.clone(),
        );
        // stand the runtime supervisor up over the joiner's direct mesh, REUSING
        // the transport the ritual ran over (its Arc owns the bootstrap queues'
        // receive credentials — a fresh transport could send but never subscribe).
        // best-effort — no mesh, no transport, or a loopback mesh just means no
        // live peer link yet.
        let (mls_blob, mesh) = net_seed;
        let reused = self.join_transport.lock().ok().and_then(|mut s| s.take());
        if self.persist && !mesh.is_empty() {
            if let (Some(blob), Some(transport)) = (mls_blob, reused) {
                // hard-kill safety (2026-07-19): the bootstrap queues' receive
                // credentials exist only in this transport's memory — merge
                // them into transport.state NOW, not only on clean close (live
                // merge; materialize_workspace already wrote mls + mesh
                // synchronously)
                if let (Some(active), Some(creds)) = (
                    self.active.as_ref(),
                    molt_net::Transport::export_creds(&transport),
                ) {
                    if !active
                        .handle
                        .persist_mesh_crypto_blocking(None, Some(creds), mesh.clone())
                    {
                        tracing::error!("the mesh handover did not reach the disk");
                    }
                }
                if let Some(net) = self.build_real_net(transport, &mesh, &blob) {
                    self.teardown_net();
                    self.net = Some(net);
                }
            }
        }
        // sealed, but NOT entered (2026-08-08): the wizard's last step makes
        // the joiner back its phrase up first; `JoinFinish` enters. The seed
        // and the run stay in `session.join` for exactly that step.
        self.session.join.sealed_id = id;
        self.session.join.awaiting_ratify = false;
        self.session.join.run.outcome = 1;
        self.session.join.run.progress_pct = 100;
        self.session
            .join
            .run
            .log
            .push("✓ sealed - back up your recovery phrase to enter".to_string());
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- recover (total-loss rejoin over a molt://recover/… link) --------

    /// Begin recovering a lost seat from a coordinator-minted recovery link
    /// and the seat's recovery phrase — the rejoiner side of the recovery
    /// ritual (`recovery_ritual.md` §4), mirroring [`State::cmd_join_start`].
    /// Parses/validates the link and ARMS the recovery context (generation,
    /// link + phrase, fresh transport slot) — the two-instance tests drive a
    /// verified [`Command::NetRecoverSealed`] against that context. Until
    /// N4's Nostr transport lands there is no network to run the rejoin
    /// over, so the production path reports an honest failure right away.
    /// The self-service reattach (`detached_reattach.md` §2.3): a workspace
    /// that opened DETACHED with a verified chain announces its seat to the
    /// survivors' standing inboxes and runs the ordinary rejoiner wait — no
    /// link, no mint, no human act. One attempt per open; where the material
    /// is missing (no seed on disk, no anchors, no confirmed ratified relay)
    /// it returns `false` and the honest detached state stays. The ticketed
    /// recovery link remains the manual fallback.
    pub(crate) fn spawn_reattach(&mut self) -> bool {
        if !self.persist {
            return false;
        }
        let member = self.member();
        let Some(head) = self.chain_head.as_ref() else {
            return false;
        };
        let Some(anchored) = head
            .identities
            .iter()
            .find(|i| i.member == member)
            .map(|i| i.identity_pk.clone())
        else {
            return false;
        };
        let republic_id = head.republic_id.clone();
        let ratified = self.ratified_relays();
        if ratified.is_empty() {
            return false;
        }
        // the seat's phrase, revealed from the workspace's sealed seed — a
        // knowledge-only restore (no seed in the blob) cannot reattach
        let Some(phrase) = self
            .session
            .workspaces
            .iter()
            .find(|w| w.id == self.session.active_workspace)
            .map(|w| w.seed.clone())
            .filter(|s| !s.is_empty())
        else {
            tracing::info!("detached workspace carries no seed - reattach needs the recovery link");
            return false;
        };
        // every OTHER seat's WORKING anchor from the restored chain —
        // possibly stale; whoever kept theirs answers
        let targets: Vec<String> = head
            .identities
            .iter()
            .filter(|i| i.member != member)
            .map(|i| self.working_nostr_pk(&i.member))
            .filter(|a| !a.is_empty())
            .collect();
        if targets.is_empty() {
            return false;
        }
        // ADR-0004: dial only what this operator confirmed
        let verdicts = molt_core::relay::diagnose_invite_relays(
            &ratified,
            &self.session.settings.relays,
            self.clearnet_session,
        );
        let dial_relays: Vec<String> = verdicts
            .iter()
            .filter(|v| v.blocked.is_none())
            .map(|v| v.url.clone())
            .collect();
        if dial_relays.is_empty() {
            tracing::info!("detached workspace: no ratified relay is locally confirmed - staying detached");
            return false;
        }
        let Ok(dialer) = self.dialer_for() else {
            return false;
        };
        let Ok(self_ticket) = molt_net::invite::mint_ticket() else {
            return false;
        };
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return false;
        };
        self.recover_generation += 1;
        let generation = self.recover_generation;
        if let Some(task) = self.recover_task.take() {
            task.abort();
        }
        let handover = molt_net::invite::RecoveryHandoverV2 {
            // the SELF-ticket (the founder's third-anchor pattern): salt for
            // the fresh transport anchor, registered nowhere
            ticket: self_ticket.clone(),
            npub: targets[0].clone(),
            relays: dial_relays.clone(),
            republic_id: republic_id.clone(),
            identity_pk: anchored,
        };
        let republic = self
            .replica
            .as_ref()
            .map(|r| r.name.clone())
            .unwrap_or_default();
        self.recover_ctx = Some((
            crate::recovery::RecoveryInvite {
                republic,
                member: member.clone(),
                ticket: self_ticket,
                server: String::new(),
                queue_id: String::new(),
                wrap: String::new(),
                republic_id,
                handover: Some(handover.clone()),
            },
            phrase.clone().into(),
        ));
        self.session.recover = molt_core::RecoverState::default();
        let extra_targets = targets.get(1..).map(<[String]>::to_vec).unwrap_or_default();
        self.recover_task = Some(crate::nostr_ritual::spawn_recovery_rejoiner(
            dialer,
            crate::nostr_ritual::RecoverCtx {
                handover,
                dial_relays,
                extra_targets,
                member,
                phrase: phrase.into(),
                generation,
            },
            cmd_tx.downgrade(),
        ));
        tracing::info!("detached workspace - reattaching to the republic");
        true
    }

    /// Self-heal a STUCK-EPOCH seat (`detached_reattach.md` §2.4): triggered
    /// by the deaf-node signature (own outbox stalled + unopenable frames).
    /// Runs the ordinary self-service reattach, capped per SESSION and
    /// spaced — two devices restoring the same seat must never re-key each
    /// other in an endless ping-pong, and a misfiring detector must cost
    /// bounded churn.
    pub(crate) fn maybe_self_heal_reattach(&mut self) {
        const SPACING_SECS: u64 = 600;
        const MAX_ATTEMPTS: u32 = 3;
        if self.reattach_attempts >= MAX_ATTEMPTS {
            return;
        }
        let now = crate::now_secs();
        if self
            .last_reattach
            .is_some_and(|t| now.saturating_sub(t) < SPACING_SECS)
        {
            return;
        }
        if self.recover_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        // stamp BEFORE trying: a spawn that cannot start (no seed, no
        // anchors) must not retry on every health frame either
        self.last_reattach = Some(now);
        if self.spawn_reattach() {
            self.reattach_attempts += 1;
            tracing::warn!(
                attempt = self.reattach_attempts,
                "group key behind and outbox stalled - self-healing via reattach"
            );
            self.session.notice = "reattaching".to_string();
            self.emit_session(SessionScope::Full);
        }
    }

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
                "not an actionable recovery link - it carries no transport details".to_string(),
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
        // pool-settled gate (the join twin), BEFORE the destructive context
        // switch below: the link-carried relays are judged against a pool
        // about to change while a confirmation probe is in flight
        if !self.pending_relay_confirms.is_empty() {
            return Err(MoltError::Recover(
                crate::relay_msg::pool_verifying_reason().to_string(),
            ));
        }
        // …and so does any in-flight join or founding: a recovery is a context
        // switch like any other. Without the join invalidation a late
        // NetJoinSealed materializes a foreign republic mid-recovery; without
        // the ritual teardown an in-flight FOUNDING can still seal into the
        // recovery session via maybe_finalize (the symmetric hole).
        self.invalidate_join();
        self.abandon_ritual("the founder started a recovery instead");
        self.ritual_attestations.clear();
        // a restarted recovery supersedes the one in flight — bump the
        // incarnation so the stale task's late result is dropped
        self.recover_generation += 1;
        let generation = self.recover_generation;
        let task_phrase = phrase.clone();
        self.recover_ctx = Some((inv.clone(), phrase.into()));
        // a fresh run starts with a fresh checklist (the old republic's
        // finished list must not front-run the new coordinator's report)
        self.session.recover = molt_core::RecoverState::default();
        // a fresh transport slot for this recovery incarnation, the
        // join_transport twin: cmd_net_recover_sealed stands the runtime
        // supervisor up from it when a rejoin task filled it (the two-instance
        // tests inject NetRecoverSealed against exactly this armed context)
        self.recover_transport = std::sync::Arc::new(std::sync::Mutex::new(None));
        self.session.notice = format!("recover-started:{}", inv.member);
        self.emit_session(SessionScope::Full);
        // A v2 handover is what makes a link runnable over Nostr (N4b step
        // 6e). Without one there is nothing to dial — a legacy queue link
        // names an SMP server this build no longer speaks to — so that path
        // keeps failing honestly, and the armed context still lets an
        // injected `NetRecoverSealed` materialize (the loopback test seam).
        let Some(handover) = inv.handover.clone() else {
            return self
                .cmd_net_recover_failed(crate::LEGACY_RECOVERY_LINK.to_string(), Some(generation));
        };
        let dialer = match self.dialer_for() {
            Ok(d) => d,
            Err(e) => {
                return self.cmd_net_recover_failed(format!("transport: {e}"), Some(generation))
            }
        };
        // What the COORDINATOR listens on, narrowed to what this operator
        // confirmed (ADR-0004: a pasted link never makes us dial somewhere
        // unapproved). One relay in common is enough.
        let verdicts = molt_core::relay::diagnose_invite_relays(
            &handover.relays,
            &self.session.settings.relays,
            self.clearnet_session,
        );
        let dial_relays: Vec<String> = verdicts
            .iter()
            .filter(|v| v.blocked.is_none())
            .map(|v| v.url.clone())
            .collect();
        if dial_relays.is_empty() {
            let refusal = crate::relay_msg::join_relay_refusal(
                &verdicts,
                &self.session.settings.relays,
                self.clearnet_session,
            );
            // R2, the recover leg: this surface has no run log, so the
            // per-relay diagnosis rides the one notice line — a rejoiner
            // told only "add one of them" cannot know WHICH (rule 5, and
            // rule 3 makes re-join the routine path for a relay change)
            let detail = refusal.detail.join(" · ");
            let error = if detail.is_empty() {
                refusal.headline
            } else {
                format!("{} · {}", refusal.headline, detail)
            };
            return self.cmd_net_recover_failed(error, Some(generation));
        }
        let Some(cmd_tx) = self.cmd_tx.upgrade() else {
            return self.cmd_net_recover_failed("engine stopped".to_string(), Some(generation));
        };
        // a restarted recovery aborts the previous incarnation's task: its
        // generation-guarded results would be dropped anyway, and aborting
        // also releases its relay sockets
        if let Some(task) = self.recover_task.take() {
            task.abort();
        }
        self.recover_task = Some(crate::nostr_ritual::spawn_recovery_rejoiner(
            dialer,
            crate::nostr_ritual::RecoverCtx {
                handover,
                dial_relays,
                extra_targets: Vec::new(),
                member: inv.member.clone(),
                phrase: task_phrase.into(),
                generation,
            },
            cmd_tx.downgrade(),
        ));
        Ok(Reply::Ack)
    }

    /// The off-actor rejoin task finished: the seat is back inside the MLS
    /// group and holds the coordinator-served chain, verified from its
    /// genesis. The actor **re-verifies everything** before materializing
    /// (defence in depth against a forged internal command — symmetric with
    /// [`State::cmd_net_join_sealed`]), then writes the recovered workspace,
    /// adopting the FULL chain, and enters the republic. Option A: no live
    /// mesh yet — re-meshing is the separate dynamic-membership feature.
    #[allow(clippy::too_many_arguments)] // one internal command, one handler
    pub(crate) fn cmd_net_recover_sealed(
        &mut self,
        member: String,
        chain: String,
        mls: String,
        mesh: Vec<molt_core::MeshLink>,
        nostr_sk: String,
        rotation_seed: String,
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
        // against the blob (founding-bound anchor, double-apply seed). The
        // recovery LINK is the external republic anchor.
        let (head, sealed) =
            match crate::chain::verify_served(checkpoint_blob.as_ref(), &blocks, Some(&inv.republic_id)) {
                Ok(pair) => pair,
                Err(e) => return self.cmd_net_recover_failed(e, generation),
            };
        // the chain must be THIS recovery's republic (no swapping in another)
        if head.republic_id != inv.republic_id || member != inv.member {
            return self.cmd_net_recover_failed(
                "the recovered chain does not match the recovery link".to_string(),
                generation,
            );
        }
        // the seat's identity re-derives from the phrase exactly as the ritual
        // anchored it — resolving the founder-vs-joiner salt against the
        // VERIFIED HEAD's anchor (WP7; a Membership block may have evolved
        // the key past the genesis — e.g. this very re-admission)
        let Some(anchored) = head
            .identities
            .iter()
            .find(|i| i.member == member)
            .map(|i| i.identity_pk.clone())
        else {
            return self.cmd_net_recover_failed(
                "the recovered chain does not anchor this seat".to_string(),
                generation,
            );
        };
        // the resolver guarantees pk == anchored (non-empty hint), so only
        // the signing key travels on
        let (sk, _pk) = match crate::founding::seat_identity(&phrase, &member, &anchored) {
            Ok(kp) => kp,
            Err(e) => return self.cmd_net_recover_failed(e, generation),
        };
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
        // --- the Nostr shape (N4b step 6d) -----------------------------------
        //
        // Each of the three pieces comes from the only place that can honestly
        // supply it, and they are deliberately NOT all taken from the task:
        //
        // - the **relay pool** from the VERIFIED chain (`sealed.relays`). The
        //   pool is chain-governed since roster-v4, so the chain is the
        //   authority; the Welcome's copy is a hint the task checked against.
        //   Sealing the task's list instead would let a coordinator narrow a
        //   recovering seat's view of its own republic.
        // - the **rotation seed** from the task, because only the Welcome
        //   carries it — the chain has no record of the h-tag seed.
        // - the **transport secret** re-derived HERE from `(phrase, ticket)`,
        //   with the task's copy as the cross-check.
        //
        // A seed present makes this a Nostr recovery; absent keeps the legacy
        // queue shape (loopback tests, pre-N4 republics).
        let shape = if rotation_seed.is_empty() {
            TransportShape::default()
        } else {
            match hex::decode(&rotation_seed) {
                Ok(seed) if seed.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&seed);
                    TransportShape::nostr(sealed.relays.clone(), arr)
                }
                _ => {
                    return self.cmd_net_recover_failed(
                        "the rejoin task delivered a malformed rotation seed".to_string(),
                        generation,
                    )
                }
            }
        };
        // The returning seat's NEW anchor. It is re-derived rather than read
        // off the roster ON PURPOSE: the roster entry is the seat's DEAD
        // founding anchor, salted with a ticket that died with the device, so
        // comparing against it would either always fail — or get "fixed" by
        // deleting the comparison, which silently accepts any key at all.
        let recovered_nostr_sk = if nostr_sk.is_empty() {
            None
        } else {
            let entropy = match molt_storage::seed_entropy(&phrase) {
                Ok(e) => e,
                Err(e) => return self.cmd_net_recover_failed(e.to_string(), generation),
            };
            let (mut derived, derived_pk) = molt_net::nostr_identity(&entropy, &inv.ticket);
            let raw = derived.to_vec();
            zeroize::Zeroize::zeroize(&mut derived);
            let delivered_ok = hex::decode(&nostr_sk)
                .ok()
                .filter(|d| d.len() == 32)
                .and_then(|d| molt_net::nostr_pk_for_sk(&d).ok())
                .is_some_and(|pk| pk == derived_pk);
            if !delivered_ok {
                return self.cmd_net_recover_failed(
                    "the rejoin task's transport secret is not this recovery's derived key"
                        .to_string(),
                    generation,
                );
            }
            // …and if the served chain already SAYS what our anchor is, it
            // must say this. The WORKING anchor, not the roster:
            // `apply_membership` keeps the seat's anchored identity key across
            // a `Restored` block, so the roster still carries the dead
            // founding anchor by design.
            //
            // Normally it says nothing yet, and that is not a weakness in the
            // check — it is the shape of the flow. A Nostr coordinator serves
            // the chain ANCHOR (the smallest prefix that verifies), and this
            // seat's own `Restored` block is at the HEAD; it arrives over the
            // ordinary catch-up once the workspace exists (§3.1). Demanding it
            // here would refuse every real recovery. What this DOES catch is a
            // coordinator serving a chain that re-anchors this seat somewhere
            // else.
            let served = crate::chain::working_anchors(&blocks);
            if served.get(&member).is_some_and(|pk| *pk != derived_pk) {
                return self.cmd_net_recover_failed(
                    "the recovered chain anchors this seat to a different transport key"
                        .to_string(),
                    generation,
                );
            }
            Some(raw)
        };
        // A LOCAL copy of this seat must not block its recovery (field flow
        // 2026-08-24): an S3-restored (detached) workspace — or an earlier
        // recovery — shares the id `create_workspace` derives, and refusing
        // on it killed the exact path the detached notice recommends. The
        // copy retires to the trash (recoverable 30 days); the verified
        // recovered state replaces it. Closed first when it is the open one.
        if self.persist {
            if let Ok(entropy) = molt_storage::seed_entropy(&phrase) {
                let ws_id = molt_storage::derive_workspace_id(&entropy, &member);
                let root = self.workspace_root();
                if let Some(dir) = molt_storage::find_workspace_dir(&root, &ws_id) {
                    if self.active.as_ref().is_some_and(|a| a.id == ws_id) {
                        self.close_active_storage();
                        self.session.active_workspace = String::new();
                    }
                    match molt_storage::trash_workspace(&root, &dir) {
                        Ok(_trashed) => {
                            self.session.workspaces.retain(|w| w.id != ws_id);
                            self.reclassify_backups();
                            tracing::info!(%ws_id, "recovery replaces the local copy - retired to the trash");
                        }
                        Err(e) => {
                            return self.cmd_net_recover_failed(
                                format!("cannot retire the local copy of this seat: {e}"),
                                generation,
                            )
                        }
                    }
                }
            }
        }
        // keep copies to stand the queue-mesh supervisor up after
        // materialising (the Nostr group runtime is materialize's own job)
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
            sealed.features.clone(),
            shape,
            Some(sk),
            recovered_nostr_sk,
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
        // recovery is NOT a full-roster live seal (unlike founding/join): only
        // the returning seat itself and the peers the re-established mesh
        // actually reached (`net_seed.1`, each an MLS-authenticated announce —
        // which captures the welcomer whenever it re-meshed with us) exchanged
        // real traffic. Everyone else is unheard: NEVER-seen, exactly like the
        // restore path — never a fabricated "seen just now". An empty mesh
        // (option A: state restored, no live links) leaves only the seat.
        let now = self.presence_now();
        let me = member.clone();
        let seen: std::collections::BTreeSet<MemberId> =
            net_seed.1.iter().map(|l| l.member.clone()).collect();
        let members = roster_members(&sealed.roster, now, |m| {
            if m == me || seen.contains(m) {
                now
            } else {
                MemberInfo::NEVER
            }
        });
        self.push_workspace_entry(
            &id,
            &sealed.name,
            sealed.rule_m,
            sealed.roster.len(),
            members,
            (*phrase).clone(),
            self.effective_net_label(),
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
                // hard-kill safety (2026-07-19): the fresh mesh queues'
                // receive credentials exist only in this transport's memory —
                // merge them into transport.state NOW, not only on clean
                // close (live merge; the join-tail twin above does the same)
                if let (Some(active), Some(creds)) = (
                    self.active.as_ref(),
                    molt_net::Transport::export_creds(&transport),
                ) {
                    if !active
                        .handle
                        .persist_mesh_crypto_blocking(None, Some(creds), mesh.clone())
                    {
                        tracing::error!("the mesh handover did not reach the disk");
                    }
                }
                if let Some(net) = self.build_real_net(transport, &mesh, &blob) {
                    self.teardown_net();
                    self.net = Some(net);
                }
            }
        }
        // On a NOSTR republic the group runtime is ALREADY UP here:
        // `materialize_workspace` brings it up for every Nostr
        // materialization — founding, join and recovery alike — and sets
        // `net_health` honestly. Building it AGAIN here (as this arm once
        // did) spawned a second outbox over the same log next to the first,
        // and every frame of the rejoiner went out twice.
        //
        // **Ask for everything above the anchor.** §3.1 says the rest arrives
        // over the ordinary catch-up, and nothing was issuing the request: the
        // two existing triggers are a gap-block arriving and a workspace OPEN,
        // and a recovery hits neither. It cannot hit the first, either — the
        // coordinator's own head block was published at the epoch BEFORE the
        // re-key, which a rejoiner that joined at the new one can never
        // decrypt (an exporter ring reaches backward only).
        //
        // After the runtime, because the request is an envelope the outbox has
        // to carry.
        if let Some(height) = self.chain_head.as_ref().map(|h| h.height) {
            self.request_catchup(height + 1);
        }
        // the seat is provably back (the Restored block sealed at m) — the
        // checklist finishes deterministically even when the last progress
        // frame lost the race against the Welcome
        if self.session.recover.member == member {
            for seat in &mut self.session.recover.seats {
                seat.approved = true;
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

    /// A join failed: surface the reason in the join run (retryable).
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
        self.join_backup = None;
        self.session.join.run.log.push(format!("✗ join failed: {error}"));
        // the headline is what the operator READS: a few words, large, in the
        // signal colour. The sentence above stays in the log for the detail.
        self.session.join.run.headline = crate::relay_msg::headline_for(&error);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// Abandon any in-flight join: bump the generation so late results are
    /// dropped, close the ratification gate (a paused ritual then declines),
    /// abort the member task, and clear the wizard.
    ///
    /// **Every entry point that switches the session out of a join must call
    /// this.** The ONLY gate on `cmd_net_join_sealed` is the join generation,
    /// so a `NetJoinSealed` from a join the user walked away from otherwise
    /// materializes a republic they never created, re-points
    /// `active_workspace` at it and flips the screen — a session hijack, not
    /// merely a stale-state bug. `cmd_join_start`/`cmd_join_cancel` did this
    /// correctly; founding, recovery and open-workspace did not.
    ///
    /// Aborting the task is deliberate and matches cancel: this join's own
    /// last outbound frame may be lost. That is the OTHER republic's founder's
    /// problem to surface (cluster F), never a reason to keep the task alive.
    /// Abandon an in-flight RECOVERY — the [`State::invalidate_join`] twin.
    ///
    /// It had nothing to abandon until step 6e: `cmd_recover_start` used to
    /// fail immediately, so an abandoned recovery cost nothing and the
    /// asymmetry (recovery invalidates join, never the other way round) was
    /// invisible. A rejoiner task holds a 1059 inbox and a 445 subscription
    /// for up to fifteen minutes, so the same context switches that abandon a
    /// join must abandon this too — a forgotten task sits on relay sockets
    /// long after the human moved on.
    pub(crate) fn invalidate_recovery(&mut self) {
        self.recover_generation += 1;
        self.recover_ctx = None;
        if let Some(task) = self.recover_task.take() {
            task.abort();
        }
    }

    pub(crate) fn invalidate_join(&mut self) {
        self.join_generation += 1;
        self.join_confirm = None;
        self.join_backup = None;
        if let Some(task) = self.join_task.take() {
            task.abort();
        }
        self.session.join = JoinState::default();
        // a fresh transport slot: the abandoned incarnation's must not be
        // inherited by whatever runs next
        self.join_transport = std::sync::Arc::new(std::sync::Mutex::new(None));
    }

    /// The join task reports a NON-FATAL transport condition (the joiner's
    /// twin of `cmd_net_ritual_note`). Never fails the run; deduped against
    /// the last line so a repeating deaf note cannot stack.
    pub(crate) fn cmd_net_join_note(
        &mut self,
        note: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        if self.session.join.run.log.last() == Some(&note) {
            return Ok(Reply::Ack);
        }
        self.session.join.run.log.push(note);
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    /// The rejoiner's status line ([`Command::NetRecoverNote`]): where the
    /// bounded recovery wait stands. Notice-borne (`recover-note:`), so the
    /// recover pane shows it live; a stale incarnation's note is dropped.
    pub(crate) fn cmd_net_recover_note(
        &mut self,
        note: String,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.recover_generation) {
            return Ok(Reply::Ack);
        }
        let notice = format!("recover-note:{note}");
        if self.session.notice != notice {
            self.session.notice = notice;
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// The rejoiner's re-admission checklist ([`Command::NetRecoverProgress`],
    /// `recovery_auto_approval.md` §4): the coordinator's report of who has
    /// approved, rendered as [`molt_core::RecoverState`]. Display data only;
    /// a stale incarnation's report is dropped like a stale note.
    pub(crate) fn cmd_net_recover_progress(
        &mut self,
        member: String,
        need: u32,
        roster: Vec<String>,
        approved: Vec<String>,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.recover_generation) {
            return Ok(Reply::Ack);
        }
        let state = molt_core::RecoverState {
            member,
            need,
            seats: roster
                .into_iter()
                .map(|m| molt_core::RecoverSeat {
                    approved: approved.contains(&m),
                    member: m,
                })
                .collect(),
        };
        if self.session.recover != state {
            self.session.recover = state;
            self.emit_session(SessionScope::Full);
        }
        Ok(Reply::Ack)
    }

    /// Enter the republic a completed join sealed — the joiner twin of
    /// [`Self::cmd_create_finish`] (2026-08-08): the seal materializes and
    /// stands the runtime up, but entering waits for the human's
    /// phrase-backup confirmation.
    pub(crate) fn cmd_join_finish(&mut self) -> Result<Reply, MoltError> {
        if self.session.join.run.outcome != 1 || self.session.join.sealed_id.is_empty() {
            return Err(MoltError::Join(
                "no sealed join awaits entry".to_string(),
            ));
        }
        let id = self.session.join.sealed_id.clone();
        self.session.join = JoinState::default();
        self.session.active_workspace = id;
        self.session.screen = Screen::Main;
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    pub(crate) fn cmd_join_cancel(&mut self) -> Result<Reply, MoltError> {
        self.invalidate_join();
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
        features: Option<Vec<String>>,
        generation: Option<u64>,
    ) -> Result<Reply, MoltError> {
        if generation != Some(self.join_generation) || self.session.join.run.outcome != 0 {
            return Ok(Reply::Ack);
        }
        self.session.join.proposed_name = name.clone();
        self.session.join.proposed_agenda = agenda;
        self.session.join.proposed_features = features;
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
        // straight into the backup step (❻½): the signature is on its way,
        // the ritual now waits for THIS member's phrase proof
        self.session.join.awaiting_backup = true;
        self.session.join.run.progress_pct = 88;
        self.session
            .join
            .run
            .log
            .push("✓ you ratified the charter · sealing your signature".to_string());
        self.session
            .join
            .run
            .log
            .push("→ save your recovery phrase - re-type it to confirm".to_string());
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
        // terminal for the whole ritual, not just this join: a declined seat
        // can never seal, so the founder must re-mint — say so here too
        self.session.join.run.log.push(
            "✗ the ritual is over - this republic must be founded anew".to_string(),
        );
        self.emit_session(SessionScope::Full);
        Ok(Reply::Ack)
    }

    // ---- the shared self-ticker ------------------------------------------

    /// Drive a periodic engine-internal command (presence aging, the backup
    /// ticker) from the engine itself — co-equal: it runs no matter which
    /// operator is attached.
    ///
    /// The task keeps only the actor's WEAK self-handle and upgrades it
    /// per tick: a long-lived ticker holding a strong sender would be the
    /// reference cycle that keeps the actor alive after the last operator
    /// handle is gone (see `engine_and_mesh_shut_down_when_the_last_handle_drops`).
    pub(crate) fn spawn_ticker_every(&self, tick: Command, period_ms: u64) {
        let weak = self.cmd_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(period_ms)).await;
                // a stopping/stopped actor ends the ticker
                let Some(tx) = weak.upgrade() else {
                    break;
                };
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
                drop(tx);
                match rx.await {
                    Ok(Ok(_)) => {}
                    _ => break,
                }
            }
        });
    }
}

// ---- the off-actor restore task (story 13) --------------------------------

/// Hard cap on a restore download (an export blob has a known scale — a
/// server claiming more is refused before a byte lands). The auto-backup
/// build side enforces the SAME cap (`backup.rs`), so it can never ship a
/// blob its own restore path would refuse at disaster time.
pub(crate) const RESTORE_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// What the restore task fetches, resolved synchronously in
/// [`State::cmd_restore_start`] (config + dialer fail fast on the actor).
pub(crate) enum RestorePlan {
    /// Read a local `.molt.enc` file.
    File(std::path::PathBuf),
    /// Download from the configured bucket.
    S3 {
        /// The validated backup target.
        config: Box<molt_net::s3::S3Config>,
        /// The fail-closed dialer (Tor when configured).
        dialer: molt_net::dial::Dialer,
        /// Which object.
        pick: S3Pick,
    },
}

/// The s3-way target: an explicit object key, or the newest object of one
/// workspace's prefix (design §6.6: empty/id target → newest).
pub(crate) enum S3Pick {
    /// A full `molt/<id>/<ts>.molt.enc` key.
    Object(String),
    /// The newest backup of this workspace-id pseudonym.
    NewestOf(String),
}

/// Send one engine-internal command (fire-and-forget reply).
async fn send_internal(cmd_tx: &tokio::sync::mpsc::Sender<Envelope>, cmd: Command) {
    let (reply, _rx) = oneshot::channel();
    let _ = cmd_tx.send(Envelope { cmd, reply }).await;
}

/// The restore fetch+stage task: every progress line reports something
/// that actually happened; the staged result parks in `slot` and the
/// actor-side handler runs the mandatory chain verification. Failures
/// return verbatim as [`Command::NetRestoreFailed`].
async fn restore_task(
    cmd_tx: tokio::sync::mpsc::Sender<Envelope>,
    generation: u64,
    plan: RestorePlan,
    root: std::path::PathBuf,
    secret: zeroize::Zeroizing<String>,
    slot: std::sync::Arc<std::sync::Mutex<Option<molt_storage::import::ImportStaging>>>,
) {
    let progress = |pct: u8, line: String| {
        send_internal(
            &cmd_tx,
            Command::NetRestoreProgress {
                pct,
                line,
                generation: Some(generation),
            },
        )
    };
    let outcome: Result<(), String> = async {
        let blob: Vec<u8> = match plan {
            RestorePlan::File(path) => {
                progress(5, format!("→ fs: read {}", path.display())).await;
                let read_path = path.clone();
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                    // the same cap the download path enforces — a crafted
                    // "blob" must not buffer unbounded memory either way
                    let len = std::fs::metadata(&read_path)
                        .map_err(|e| format!("reading {}: {e}", read_path.display()))?
                        .len();
                    if len > RESTORE_MAX_BYTES {
                        return Err(format!(
                            "file is {len} bytes - beyond the {RESTORE_MAX_BYTES}-byte cap"
                        ));
                    }
                    std::fs::read(&read_path)
                        .map_err(|e| format!("reading {}: {e}", read_path.display()))
                })
                .await
                .map_err(|e| format!("read task failed: {e}"))??
            }
            RestorePlan::S3 { config, dialer, pick } => {
                let client = molt_net::s3::S3Client::new(*config, dialer);
                let object = match pick {
                    S3Pick::Object(key) => key,
                    S3Pick::NewestOf(id) => {
                        let prefix = format!("{}{id}/", molt_core::BACKUP_OBJECT_PREFIX);
                        progress(3, format!("→ s3: list {prefix}")).await;
                        let listed = client
                            .list_objects(&prefix)
                            .await
                            .map_err(|e| format!("listing the bucket failed: {e}"))?;
                        // lexicographic max IS the newest (§6.2 zero-padded keys)
                        let mut keys: Vec<String> = listed
                            .into_iter()
                            .filter(|o| {
                                molt_core::parse_backup_key(&o.key)
                                    .is_some_and(|(kid, _)| kid == id)
                            })
                            .map(|o| o.key)
                            .collect();
                        keys.sort_unstable();
                        keys.pop().ok_or_else(|| {
                            format!("no backup for workspace {id} in the bucket")
                        })?
                    }
                };
                progress(8, format!("→ s3: GET {object}")).await;
                let mut sink: Vec<u8> = Vec::new();
                // the download callback is synchronous — progress rides a
                // lossy try_send (a dropped percent line is fine; the final
                // staged/failed report never travels this path)
                let tx = cmd_tx.clone();
                let mut last_pct = 8u8;
                client
                    .get_object(&object, &mut sink, RESTORE_MAX_BYTES, &mut |done, total| {
                        let Some(total) = total.filter(|t| *t > 0) else {
                            return;
                        };
                        let pct =
                            10u8.saturating_add(u8::try_from(done * 50 / total).unwrap_or(50));
                        if pct >= last_pct.saturating_add(5) || (done == total && pct > last_pct)
                        {
                            last_pct = pct;
                            let (reply, _rx) = oneshot::channel();
                            let _ = tx.try_send(Envelope {
                                cmd: Command::NetRestoreProgress {
                                    pct,
                                    line: format!("↓ {done} of {total} bytes"),
                                    generation: Some(generation),
                                },
                                reply,
                            });
                        }
                    })
                    .await
                    .map_err(|e| format!("download failed: {e}"))?;
                sink
            }
        };
        progress(60, "→ decrypting + validating the blob".to_string()).await;
        // blocking: Argon2 (passphrase blobs) + staging I/O
        let stage_root = root.clone();
        let staging = tokio::task::spawn_blocking(move || {
            molt_storage::import::import_stage(&stage_root, &blob, &secret)
        })
        .await
        .map_err(|e| format!("staging task failed: {e}"))?
        .map_err(|e| e.to_string())?;
        progress(
            85,
            format!(
                "→ staged · {} chain block(s) await verification",
                staging.chain.len()
            ),
        )
        .await;
        if let Ok(mut s) = slot.lock() {
            *s = Some(staging);
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => {
            send_internal(
                &cmd_tx,
                Command::NetRestoreStaged {
                    generation: Some(generation),
                },
            )
            .await;
        }
        Err(error) => {
            send_internal(
                &cmd_tx,
                Command::NetRestoreFailed {
                    error,
                    generation: Some(generation),
                },
            )
            .await;
        }
    }
}
