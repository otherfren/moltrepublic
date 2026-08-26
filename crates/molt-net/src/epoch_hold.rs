// SPDX-License-Identifier: GPL-3.0-or-later

//! **The cross-epoch retry loop** both inbound paths share (review M12).
//!
//! A frame encrypted at an epoch this node has not reached is HELD until the
//! commit that unlocks it merges; after every merge the hold is re-offered
//! in hold order, and repeatedly while a pass makes progress — a held commit
//! can unlock further held frames, so one pass is never enough.
//!
//! The eviction rule (review M5) lives here, once: a frame still opaque
//! after a pass that made progress is NOT yet opaque for good — a laggard
//! two epochs behind holds frames sealed under the epoch the NEXT held
//! commit opens — so it stays held; only the terminating no-progress pass
//! counts what it could not open as lost. The mesh queue path
//! (`supervisor::drain_epoch_buffer`) and the 445 group runtime
//! (`group_runtime::retry_epoch_hold`) differ only in what a held item is
//! and what "ingest one" does; they say so through [`HeldIngest`].

/// What ingesting one held item turned into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// Consumed, and the pass made progress (something decoded or a commit
    /// merged) — the hold is worth another pass.
    Progress,
    /// Consumed without progress (the item had nothing for this leg).
    Consumed,
    /// Still ahead of this node's epoch — keep holding, never counted.
    Still,
    /// Nothing this node holds opens it. Held while the pass progresses,
    /// counted lost by the terminating pass.
    Opaque,
    /// The engine is gone: the caller must stop.
    Stop,
}

/// The one thing each inbound path supplies: how to ingest one held item.
pub(crate) trait HeldIngest<T> {
    async fn ingest(&mut self, item: &mut T) -> Held;
}

/// Re-offer `hold` while progress is made. Returns the items the
/// terminating pass found opaque (lost for good — the caller counts or acks
/// them); `Err(())` when the ingest asked to stop. Hold order is kept across
/// passes (it is the sender-ratchet generation order).
pub(crate) async fn drain_until_no_progress<T, I: HeldIngest<T>>(
    hold: &mut Vec<T>,
    ingest: &mut I,
) -> Result<Vec<T>, ()> {
    loop {
        let mut progress = false;
        // (item, opaque this pass) — in arrival order
        let mut kept: Vec<(T, bool)> = Vec::with_capacity(hold.len());
        for mut item in std::mem::take(hold) {
            match ingest.ingest(&mut item).await {
                Held::Progress => progress = true,
                Held::Consumed => {}
                Held::Still => kept.push((item, false)),
                Held::Opaque => kept.push((item, true)),
                Held::Stop => return Err(()),
            }
        }
        if progress {
            hold.extend(kept.into_iter().map(|(item, _)| item));
            if hold.is_empty() {
                return Ok(Vec::new());
            }
            continue;
        }
        let mut lost = Vec::new();
        for (item, opaque) in kept {
            if opaque {
                lost.push(item);
            } else {
                hold.push(item);
            }
        }
        return Ok(lost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An item that answers a scripted sequence of outcomes, one per pass.
    struct Scripted {
        name: &'static str,
        script: Vec<Held>,
    }

    struct Replay;
    impl HeldIngest<Scripted> for Replay {
        async fn ingest(&mut self, item: &mut Scripted) -> Held {
            if item.script.is_empty() {
                return Held::Still;
            }
            item.script.remove(0)
        }
    }

    fn item(name: &'static str, script: &[Held]) -> Scripted {
        Scripted {
            name,
            script: script.to_vec(),
        }
    }

    fn names(items: &[Scripted]) -> Vec<&'static str> {
        items.iter().map(|i| i.name).collect()
    }

    /// M5: a frame opaque in a pass that merged a commit is retried by the
    /// next pass — and opens there.
    #[tokio::test]
    async fn an_opaque_frame_survives_a_progressing_pass() {
        let mut hold = vec![
            item("a", &[Held::Opaque, Held::Progress]),
            item("commit", &[Held::Progress]),
            item("c", &[Held::Still, Held::Still, Held::Still]),
        ];
        let lost = drain_until_no_progress(&mut hold, &mut Replay).await.expect("no stop");
        assert!(lost.is_empty(), "nothing was lost: `a` opened in the second pass");
        assert_eq!(names(&hold), vec!["c"], "the future-epoch frame stays held");
    }

    /// The terminating pass counts what it could not open — and only that.
    #[tokio::test]
    async fn the_terminating_pass_counts_the_opaque_frames_as_lost() {
        let mut hold = vec![
            item("a", &[Held::Opaque, Held::Opaque]),
            item("commit", &[Held::Progress]),
            item("c", &[Held::Still, Held::Still]),
            item("d", &[Held::Opaque, Held::Opaque]),
        ];
        let lost = drain_until_no_progress(&mut hold, &mut Replay).await.expect("no stop");
        assert_eq!(names(&lost), vec!["a", "d"], "opaque after a no-progress pass = lost");
        assert_eq!(names(&hold), vec!["c"]);
    }

    /// Hold order is arrival order across passes (the sender-ratchet
    /// generation order), whatever each item answered.
    #[tokio::test]
    async fn hold_order_is_kept_across_passes() {
        let mut hold = vec![
            item("x", &[Held::Still, Held::Still]),
            item("y", &[Held::Opaque, Held::Still]),
            item("commit", &[Held::Progress]),
            item("z", &[Held::Still, Held::Still]),
        ];
        let lost = drain_until_no_progress(&mut hold, &mut Replay).await.expect("no stop");
        assert!(lost.is_empty());
        assert_eq!(names(&hold), vec!["x", "y", "z"]);
    }

    /// A pass that consumes everything ends the drain empty-handed.
    #[tokio::test]
    async fn an_emptied_hold_ends_the_drain() {
        let mut hold = vec![item("a", &[Held::Progress]), item("b", &[Held::Consumed])];
        let lost = drain_until_no_progress(&mut hold, &mut Replay).await.expect("no stop");
        assert!(lost.is_empty());
        assert!(hold.is_empty());
    }

    /// A stop ends the drain at once.
    #[tokio::test]
    async fn a_stop_ends_the_drain() {
        let mut hold = vec![item("a", &[Held::Stop]), item("b", &[Held::Progress])];
        assert!(drain_until_no_progress(&mut hold, &mut Replay).await.is_err());
    }
}
