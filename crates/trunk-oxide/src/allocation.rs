//! Who gets a channel.
//!
//! Every call that wants a channel, and every control channel, is a
//! [`Want`]. There are `budget` channels. The allocation is the prefix of
//! the wants in rank order: lower priority number first; at equal
//! priority, a holder before a newcomer (so nothing is evicted for an
//! equal); then the longest waiting; then the lowest id, so the order is
//! total. The executor applies the difference between one allocation and
//! the next: a want that falls out of the prefix is evicted, one that
//! enters is admitted.
//!
//! Because a holder only gains rank and a newcomer only loses it, the
//! allocation is a fixed point (allocating again from it changes nothing),
//! and one want added, removed or changed moves at most one call in and one
//! out.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use crate::call_state::CallId;

/// A channel claim's priority: the lower number wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(pub i32);

impl Default for Priority {
    fn default() -> Self {
        Self::UNSET
    }
}

impl Priority {
    /// After every configured priority.
    pub const UNSET: Self = Self(i32::MAX);

    /// The configured value, or [`Self::UNSET`].
    #[must_use]
    pub const fn or_unset(p: Option<i32>) -> Self {
        match p {
            Some(p) => Self(p),
            None => Self::UNSET,
        }
    }
}

/// A claim on a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Want {
    pub(crate) priority: Priority,
    /// When it was first made.
    pub(crate) since: Instant,
}

/// The order wants are served in (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    priority: Priority,
    newcomer: bool,
    since: Instant,
    id: CallId,
}

/// The wants that get one of `budget` channels, given who `held` them.
pub(crate) fn allocate(
    wants: &BTreeMap<CallId, Want>,
    held: &BTreeSet<CallId>,
    budget: usize,
) -> BTreeSet<CallId> {
    let mut ranked: Vec<Rank> = wants
        .iter()
        .map(|(&id, w)| Rank {
            priority: w.priority,
            newcomer: !held.contains(&id),
            since: w.since,
            id,
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().take(budget).map(|r| r.id).collect()
}

/// What changes from one allocation to the next.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Diff {
    pub(crate) close: Vec<CallId>,
    pub(crate) open: Vec<CallId>,
}

pub(crate) fn diff(before: &BTreeSet<CallId>, after: &BTreeSet<CallId>) -> Diff {
    Diff {
        close: before.difference(after).copied().collect(),
        open: after.difference(before).copied().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration;

    fn wants(spec: &[(u64, i32, u64)]) -> BTreeMap<CallId, Want> {
        let t0 = Instant::now();
        spec.iter()
            .map(|&(id, p, at)| {
                (
                    CallId(id),
                    Want {
                        priority: Priority(p),
                        since: t0 + Duration::from_millis(at),
                    },
                )
            })
            .collect()
    }

    fn ids(v: &[u64]) -> BTreeSet<CallId> {
        v.iter().map(|&i| CallId(i)).collect()
    }

    #[test]
    fn lower_priority_numbers_win_and_holders_beat_equal_newcomers() {
        // 1: public works (3), 2: dispatch (1), 3: another public works call (3)
        let w = wants(&[(1, 3, 0), (2, 1, 5), (3, 3, 1)]);
        assert_eq!(allocate(&w, &ids(&[]), 2), ids(&[1, 2]), "oldest of equals");
        assert_eq!(
            allocate(&w, &ids(&[3]), 2),
            ids(&[2, 3]),
            "the holder keeps its channel against an older equal"
        );
        // dispatch arrives with both public works calls on the air
        assert_eq!(
            allocate(&w, &ids(&[1, 3]), 2),
            ids(&[1, 2]),
            "evicts the newer"
        );
        assert_eq!(allocate(&w, &ids(&[1, 3]), 0), ids(&[]));
        assert_eq!(allocate(&w, &ids(&[1, 3]), 9), ids(&[1, 2, 3]));
        assert_eq!(
            diff(&ids(&[1, 3]), &ids(&[1, 2])),
            Diff {
                close: vec![CallId(3)],
                open: vec![CallId(2)]
            }
        );
        assert!(Priority(3) < Priority::UNSET);
        assert_eq!(Priority::or_unset(None), Priority::UNSET);
    }

    /// Wants on a small id space, with some of them held.
    fn scenario() -> impl Strategy<Value = (BTreeMap<CallId, Want>, BTreeSet<CallId>, usize)> {
        (
            prop::collection::btree_map(0u64..24, (0i32..4, 0u64..50), 0..16),
            prop::collection::btree_set(0u64..24, 0..12),
            0usize..10,
        )
            .prop_map(|(w, held, budget)| {
                let spec: Vec<_> = w.into_iter().map(|(id, (p, t))| (id, p, t)).collect();
                (wants(&spec), held.into_iter().map(CallId).collect(), budget)
            })
    }

    proptest! {
        #[test]
        fn the_allocation_is_the_rank_prefix_within_budget((w, held, budget) in scenario()) {
            let a = allocate(&w, &held, budget);
            prop_assert_eq!(a.len(), budget.min(w.len()));
            prop_assert!(a.iter().all(|id| w.contains_key(id)));
            // nothing left out outranks anything let in
            let rank = |id: &CallId| Rank {
                priority: w[id].priority,
                newcomer: !held.contains(id),
                since: w[id].since,
                id: *id,
            };
            for out in w.keys().filter(|id| !a.contains(id)) {
                for inn in &a {
                    prop_assert!(rank(inn) < rank(out));
                }
            }
        }

        #[test]
        fn allocating_from_an_allocation_changes_nothing((w, held, budget) in scenario()) {
            let a = allocate(&w, &held, budget);
            prop_assert_eq!(allocate(&w, &a, budget), a);
        }

        #[test]
        fn one_want_changed_moves_at_most_one_in_and_one_out(
            (w, held, budget) in scenario(),
            id in 0u64..24,
            change in prop::option::of((0i32..4, 0u64..50)),
        ) {
            let settled = allocate(&w, &held, budget);
            let mut w2 = w.clone();
            match change {
                Some((p, t)) => {
                    w2.insert(CallId(id), wants(&[(id, p, t)])[&CallId(id)]);
                }
                None => {
                    w2.remove(&CallId(id));
                }
            }
            let next = allocate(&w2, &settled, budget);
            let d = diff(&settled, &next);
            // a removed holder's channel is a close of its own, not an eviction
            let evicted = d.close.iter().filter(|c| w2.contains_key(c)).count();
            prop_assert!(d.open.len() <= 1, "{:?}", d);
            prop_assert!(evicted <= 1, "{:?}", d);
            prop_assert!(d.close.len() <= 1, "{:?}", d);
        }

        #[test]
        fn a_holder_is_never_evicted_for_an_equal_or_lower_claim(
            (w, held, budget) in scenario(),
            id in 100u64..124,
            p in 0i32..4,
        ) {
            let settled = allocate(&w, &held, budget);
            let mut w2 = w.clone();
            w2.insert(CallId(id), wants(&[(id, p, 0)])[&CallId(id)]);
            let next = allocate(&w2, &settled, budget);
            for gone in settled.difference(&next) {
                prop_assert!(w2[gone].priority > Priority(p));
            }
        }
    }
}
