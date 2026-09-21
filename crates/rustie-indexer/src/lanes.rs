//! Lanes: concurrent indexing pipelines over one index.
//!
//! One indexing pipeline uses about two cores (its indexing thread is serial). To use more, the
//! indexer runs several pipelines at once, each on its **own Quickwit source** (a lane).
//! Quickwit runs one merge pipeline per source and a merge pipeline only plans merges of its own
//! source's splits, so lanes can never plan the same merge twice.
//!
//! A batch is identified by the hash of its content (its checkpoint partition). Which lane
//! indexed a batch is not part of that identity: before submitting, [`plan_batch`] looks the
//! partition up in *every* source's checkpoint, so re-runs skip finished batches and resume a
//! partly published one on the lane that holds its progress, whatever the number of lanes was
//! when it was indexed.

use quickwit_config::CLI_SOURCE_ID;
use quickwit_metastore::checkpoint::{IndexCheckpoint, PartitionId};

/// Source id prefix of the additional lanes; lane 0 is Quickwit's default CLI source.
pub(crate) const LANE_PREFIX: &str = "rustie-lane-";

/// The source id of lane `index`.
pub(crate) fn lane_source_id(index: usize) -> String {
    match index {
        0 => CLI_SOURCE_ID.to_string(),
        n => format!("{LANE_PREFIX}{n}"),
    }
}

/// What to do with a batch, given the checkpoints of all sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchPlan {
    /// Every document was already published (by some lane): skip it.
    AlreadyIndexed,
    /// Some documents were published by this source: it alone can resume without duplicating
    /// them (a source starts from its own checkpoint).
    Resume(String),
    /// Nothing published yet: any lane can take it.
    Fresh,
}

/// Documents of the batch `partition` (of `num_docs` documents) the sources have consumed.
pub(crate) fn plan_batch<S: AsRef<str>>(
    partition: &str,
    num_docs: usize,
    checkpoint: &IndexCheckpoint,
    source_ids: impl IntoIterator<Item = S>,
) -> BatchPlan {
    let partition_id = PartitionId::from(partition);
    let mut source_ids: Vec<S> = source_ids.into_iter().collect();
    source_ids.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));

    let mut furthest: Option<(usize, &str)> = None;
    for source_id in &source_ids {
        let consumed = checkpoint
            .source_checkpoint(source_id.as_ref())
            .and_then(|source| source.position_for_partition(&partition_id))
            // A vec source stores the offset of the last document it emitted.
            .and_then(|position| position.as_usize())
            .map_or(0, |last| last + 1);
        if consumed >= num_docs {
            return BatchPlan::AlreadyIndexed;
        }
        if consumed > 0 && furthest.is_none_or(|(best, _)| consumed > best) {
            furthest = Some((consumed, source_id.as_ref()));
        }
    }
    match furthest {
        Some((_, source_id)) => BatchPlan::Resume(source_id.to_string()),
        None => BatchPlan::Fresh,
    }
}

#[cfg(test)]
mod tests {
    use quickwit_metastore::checkpoint::{IndexCheckpointDelta, SourceCheckpointDelta};
    use quickwit_proto::types::Position;

    use super::*;

    /// Checkpoint in which `source` consumed the first `consumed` documents of `partition`.
    fn consume(checkpoint: &mut IndexCheckpoint, source: &str, partition: &str, consumed: usize) {
        let delta = IndexCheckpointDelta {
            source_id: source.to_string(),
            source_delta: SourceCheckpointDelta::from_partition_delta(
                PartitionId::from(partition),
                Position::Beginning,
                Position::offset(consumed - 1),
            )
            .unwrap(),
        };
        checkpoint.try_apply_delta(delta).unwrap();
    }

    #[test]
    fn lane_ids() {
        assert_eq!(lane_source_id(0), CLI_SOURCE_ID);
        assert_eq!(lane_source_id(3), "rustie-lane-3");
    }

    #[test]
    fn unseen_batches_are_fresh_and_finished_ones_skipped_whichever_lane_did_them() {
        let sources = [CLI_SOURCE_ID, "rustie-lane-1", "rustie-lane-2"];
        let mut checkpoint = IndexCheckpoint::default();
        assert_eq!(
            plan_batch("p", 10, &checkpoint, sources),
            BatchPlan::Fresh,
            "no checkpoint at all"
        );

        // Finished by lane 2; the plan does not depend on which lanes a later run uses.
        consume(&mut checkpoint, "rustie-lane-2", "p", 10);
        assert_eq!(
            plan_batch("p", 10, &checkpoint, sources),
            BatchPlan::AlreadyIndexed
        );
        assert_eq!(
            plan_batch("p", 10, &checkpoint, [CLI_SOURCE_ID]),
            BatchPlan::Fresh,
            "a source the caller does not list is not consulted"
        );
        assert_eq!(
            plan_batch("p", 10, &checkpoint, sources.iter().rev()),
            BatchPlan::AlreadyIndexed,
            "order of the source ids is irrelevant"
        );
        // Another batch is unaffected.
        assert_eq!(plan_batch("q", 10, &checkpoint, sources), BatchPlan::Fresh);
    }

    #[test]
    fn a_partly_published_batch_resumes_on_the_lane_that_holds_it() {
        let sources = [CLI_SOURCE_ID, "rustie-lane-1", "rustie-lane-2"];
        let mut checkpoint = IndexCheckpoint::default();
        consume(&mut checkpoint, "rustie-lane-1", "p", 4);
        assert_eq!(
            plan_batch("p", 10, &checkpoint, sources),
            BatchPlan::Resume("rustie-lane-1".to_string()),
            "only lane 1 can continue without indexing documents 0..4 again"
        );
        // A batch with fewer documents than were consumed counts as finished.
        assert_eq!(
            plan_batch("p", 4, &checkpoint, sources),
            BatchPlan::AlreadyIndexed
        );
        // Should two sources ever hold progress, the furthest one continues.
        consume(&mut checkpoint, "rustie-lane-2", "p", 7);
        assert_eq!(
            plan_batch("p", 10, &checkpoint, sources),
            BatchPlan::Resume("rustie-lane-2".to_string())
        );
    }
}
