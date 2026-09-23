//! Deterministic event scheduler enforcing rules S1–S6 (`docs/m0-design.md` §4.3).
//!
//! Events are dispatched in [`EventKey`] order. Every key carries a unique,
//! runtime-assigned sequence number, so the order is strict and never depends on how the
//! underlying heap arranges or breaks ties. Snapshots export the queue sorted by key and
//! restore accepts it in any order.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;

use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase};
use systemscope_contracts::time::Tick;

/// A scheduled event: its key and the payload delivered when it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event<T> {
    /// Dispatch order of this event.
    pub key: EventKey,
    /// What the event delivers.
    pub payload: T,
}

/// Session-level scheduler settings. They affect behavior, so they belong to the session
/// configuration rather than to snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// S5: the most events that may run in one `(tick, phase)`.
    pub max_events_per_phase: u64,
}

impl Default for SchedulerConfig {
    fn default() -> SchedulerConfig {
        SchedulerConfig {
            max_events_per_phase: 1_000_000,
        }
    }
}

/// The complete scheduler state, sufficient to resume dispatch exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchedulerSnapshot<T> {
    /// Key of the most recently dispatched event, if any.
    pub last_dispatched: Option<EventKey>,
    /// Events dispatched so far in `last_dispatched`'s `(tick, phase)`.
    pub dispatched_in_phase: u64,
    /// The sequence number the next scheduled event will receive.
    pub next_sequence: u64,
    /// Pending events, sorted by key.
    pub queue: Vec<Event<T>>,
}

/// Why a [`SchedulerSnapshot`] was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulerRestoreError {
    /// Two pending events share a sequence number.
    DuplicateSequence(u64),
    /// A pending event's sequence was not yet assigned according to `next_sequence`.
    SequenceNotYetAssigned(u64),
    /// A pending event is not after the last dispatched event.
    EventBeforeCursor(EventKey),
    /// A pending event is in [`Phase::Observe`].
    ObservePhaseEvent(EventKey),
    /// `dispatched_in_phase` is inconsistent with `last_dispatched` or the S5 limit.
    InvalidPhaseCount(u64),
}

impl fmt::Display for SchedulerRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulerRestoreError::DuplicateSequence(s) => write!(f, "duplicate sequence {s}"),
            SchedulerRestoreError::SequenceNotYetAssigned(s) => {
                write!(f, "sequence {s} is not below next_sequence")
            }
            SchedulerRestoreError::EventBeforeCursor(k) => {
                write!(
                    f,
                    "pending event {k:?} is not after the last dispatched event"
                )
            }
            SchedulerRestoreError::ObservePhaseEvent(k) => {
                write!(f, "pending event {k:?} is in OBSERVE")
            }
            SchedulerRestoreError::InvalidPhaseCount(n) => {
                write!(f, "invalid dispatched_in_phase {n}")
            }
        }
    }
}

impl std::error::Error for SchedulerRestoreError {}

/// Heap entry ordered by key only; payloads never influence order.
struct Entry<T>(Event<T>);

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Entry<T>) -> bool {
        self.0.key == other.0.key
    }
}

impl<T> Eq for Entry<T> {}

impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Entry<T>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Entry<T>) -> Ordering {
        self.0.key.cmp(&other.0.key)
    }
}

/// Orders, validates, and dispatches events.
pub struct Scheduler<T> {
    config: SchedulerConfig,
    last_dispatched: Option<EventKey>,
    dispatched_in_phase: u64,
    next_sequence: u64,
    heap: BinaryHeap<Reverse<Entry<T>>>,
}

impl<T> Scheduler<T> {
    /// Creates an empty scheduler at tick 0, before `REQUEST`.
    pub fn new(config: SchedulerConfig) -> Scheduler<T> {
        Scheduler {
            config,
            last_dispatched: None,
            dispatched_in_phase: 0,
            next_sequence: 0,
            heap: BinaryHeap::new(),
        }
    }

    /// The current tick: that of the last dispatched event, or 0.
    pub fn now(&self) -> Tick {
        self.last_dispatched.map_or(Tick::ZERO, |k| k.tick)
    }

    /// The current phase: that of the last dispatched event, or `REQUEST`.
    pub fn phase(&self) -> Phase {
        self.last_dispatched.map_or(Phase::Request, |k| k.phase)
    }

    /// Key of the most recently dispatched event, if any.
    pub fn last_dispatched(&self) -> Option<EventKey> {
        self.last_dispatched
    }

    /// The session limits this scheduler enforces.
    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    /// The sequence number the next scheduled event will receive.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Number of pending events.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether no events are pending.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Key of the next event to dispatch, if any.
    pub fn peek_key(&self) -> Option<EventKey> {
        self.heap.peek().map(|Reverse(e)| e.0.key)
    }

    /// Schedules `payload` at `(tick, phase)` and returns its key.
    ///
    /// Enforces S1 (not in the past), S2 (no earlier phase in the current tick),
    /// S3 (never `OBSERVE`), S4 (runtime-assigned sequence), and S6 (no wrap-around).
    /// On error the scheduler is unchanged.
    pub fn schedule(&mut self, tick: Tick, phase: Phase, payload: T) -> Result<EventKey, SimError> {
        let (now, current) = (self.now(), self.phase());
        let violation = SimError::PhaseViolation {
            now,
            current,
            tick,
            requested: phase,
        };
        if phase == Phase::Observe {
            return Err(violation);
        }
        if tick < now {
            return Err(SimError::PastTick {
                now,
                requested: tick,
            });
        }
        if tick == now && phase < current {
            return Err(violation);
        }
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).ok_or(SimError::SequenceOverflow)?;
        let key = EventKey {
            tick,
            phase,
            sequence,
        };
        self.heap.push(Reverse(Entry(Event { key, payload })));
        Ok(key)
    }

    /// Removes and returns the next event, advancing the current tick and phase to it.
    ///
    /// Enforces S5: if the next event would exceed `max_events_per_phase` in its
    /// `(tick, phase)`, it is left in the queue and an error is returned.
    pub fn pop(&mut self) -> Result<Option<Event<T>>, SimError> {
        let Some(next) = self.peek_key() else {
            return Ok(None);
        };
        let same_phase = self
            .last_dispatched
            .is_some_and(|last| last.tick == next.tick && last.phase == next.phase);
        let count = if same_phase {
            self.dispatched_in_phase + 1
        } else {
            1
        };
        if count > self.config.max_events_per_phase {
            return Err(SimError::SameTickLivelock {
                tick: next.tick,
                phase: next.phase,
                limit: self.config.max_events_per_phase,
            });
        }
        let Reverse(Entry(event)) = self.heap.pop().expect("peeked entry exists");
        self.last_dispatched = Some(event.key);
        self.dispatched_in_phase = count;
        Ok(Some(event))
    }

    /// Captures the full state, with the queue sorted by key.
    pub fn snapshot(&self) -> SchedulerSnapshot<T>
    where
        T: Clone,
    {
        let mut queue: Vec<Event<T>> = self.heap.iter().map(|Reverse(e)| e.0.clone()).collect();
        queue.sort_by_key(|e| e.key);
        SchedulerSnapshot {
            last_dispatched: self.last_dispatched,
            dispatched_in_phase: self.dispatched_in_phase,
            next_sequence: self.next_sequence,
            queue,
        }
    }

    /// Rebuilds a scheduler from a snapshot. The queue may be in any order.
    ///
    /// Rejects snapshots that no sequence of valid operations could have produced.
    pub fn restore(
        config: SchedulerConfig,
        snapshot: SchedulerSnapshot<T>,
    ) -> Result<Scheduler<T>, SchedulerRestoreError> {
        let SchedulerSnapshot {
            last_dispatched,
            dispatched_in_phase,
            next_sequence,
            mut queue,
        } = snapshot;

        let count_ok = match last_dispatched {
            None => dispatched_in_phase == 0,
            Some(_) => (1..=config.max_events_per_phase).contains(&dispatched_in_phase),
        };
        if !count_ok {
            return Err(SchedulerRestoreError::InvalidPhaseCount(
                dispatched_in_phase,
            ));
        }

        queue.sort_by_key(|e| e.key.sequence);
        for pair in queue.windows(2) {
            if pair[0].key.sequence == pair[1].key.sequence {
                return Err(SchedulerRestoreError::DuplicateSequence(
                    pair[0].key.sequence,
                ));
            }
        }
        for event in &queue {
            let key = event.key;
            if key.sequence >= next_sequence {
                return Err(SchedulerRestoreError::SequenceNotYetAssigned(key.sequence));
            }
            if key.phase == Phase::Observe {
                return Err(SchedulerRestoreError::ObservePhaseEvent(key));
            }
            if last_dispatched.is_some_and(|last| key <= last) {
                return Err(SchedulerRestoreError::EventBeforeCursor(key));
            }
        }

        let heap = queue.into_iter().map(|e| Reverse(Entry(e))).collect();
        Ok(Scheduler {
            config,
            last_dispatched,
            dispatched_in_phase,
            next_sequence,
            heap,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched() -> Scheduler<u32> {
        Scheduler::new(SchedulerConfig::default())
    }

    fn drain(s: &mut Scheduler<u32>) -> Vec<u32> {
        let mut out = Vec::new();
        while let Some(e) = s.pop().unwrap() {
            out.push(e.payload);
        }
        out
    }

    /// Dispatches until the next event is at `(tick, phase)` or later, then dispatches it.
    fn advance_to(s: &mut Scheduler<u32>, tick: u64, phase: Phase) {
        s.schedule(Tick(tick), phase, u32::MAX).unwrap();
        while s.pop().unwrap().is_some_and(|e| e.payload != u32::MAX) {}
    }

    #[test]
    fn dispatches_in_key_order() {
        let mut s = sched();
        s.schedule(Tick(2), Phase::Request, 4).unwrap();
        s.schedule(Tick(1), Phase::Commit, 3).unwrap();
        s.schedule(Tick(1), Phase::Request, 1).unwrap();
        s.schedule(Tick(1), Phase::Complete, 2).unwrap();
        s.schedule(Tick(1), Phase::Request, 5).unwrap();
        assert_eq!(drain(&mut s), [1, 5, 2, 3, 4]);
    }

    #[test]
    fn same_tick_and_phase_is_fifo() {
        let mut s = sched();
        for p in 0..100 {
            s.schedule(Tick(7), Phase::Transfer, p).unwrap();
        }
        assert_eq!(drain(&mut s), (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn sequence_is_assigned_by_scheduler() {
        let mut s = sched();
        let a = s.schedule(Tick(9), Phase::Request, 0).unwrap();
        let b = s.schedule(Tick(1), Phase::Request, 0).unwrap();
        assert_eq!((a.sequence, b.sequence), (0, 1));
        assert_eq!(s.next_sequence(), 2);
    }

    #[test]
    fn s1_rejects_past_ticks() {
        let mut s = sched();
        advance_to(&mut s, 10, Phase::Request);
        let err = s.schedule(Tick(9), Phase::Commit, 0);
        assert_eq!(
            err,
            Err(SimError::PastTick {
                now: Tick(10),
                requested: Tick(9)
            })
        );
    }

    #[test]
    fn s2_rejects_earlier_phase_in_current_tick() {
        let mut s = sched();
        advance_to(&mut s, 10, Phase::Complete);
        for phase in [Phase::Request, Phase::Transfer] {
            assert!(matches!(
                s.schedule(Tick(10), phase, 0),
                Err(SimError::PhaseViolation { .. })
            ));
        }
        // Same phase, later phase, and earlier phase of a later tick are all allowed.
        s.schedule(Tick(10), Phase::Complete, 0).unwrap();
        s.schedule(Tick(10), Phase::Commit, 0).unwrap();
        s.schedule(Tick(11), Phase::Request, 0).unwrap();
    }

    #[test]
    fn s3_rejects_observe_at_any_tick() {
        let mut s = sched();
        for tick in [0, 1, u64::MAX] {
            assert!(matches!(
                s.schedule(Tick(tick), Phase::Observe, 0),
                Err(SimError::PhaseViolation {
                    requested: Phase::Observe,
                    ..
                })
            ));
        }
        assert!(s.is_empty());
        assert_eq!(s.next_sequence(), 0);
    }

    #[test]
    fn s5_allows_exactly_the_limit_in_one_phase() {
        let limit = 5;
        let mut s = Scheduler::new(SchedulerConfig {
            max_events_per_phase: limit,
        });
        // A zero-delay chain: every event schedules the next in the same (tick, phase).
        s.schedule(Tick(3), Phase::Transfer, 0).unwrap();
        for i in 1..=limit {
            let e = s.pop().unwrap().unwrap();
            assert_eq!(e.payload, i as u32 - 1);
            s.schedule(Tick(3), Phase::Transfer, i as u32).unwrap();
        }
        assert_eq!(
            s.pop(),
            Err(SimError::SameTickLivelock {
                tick: Tick(3),
                phase: Phase::Transfer,
                limit
            })
        );
        // The blocked event stays queued.
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn s5_counter_resets_on_phase_or_tick_change() {
        let mut s = Scheduler::new(SchedulerConfig {
            max_events_per_phase: 1,
        });
        s.schedule(Tick(1), Phase::Request, 0).unwrap();
        s.schedule(Tick(1), Phase::Transfer, 1).unwrap();
        s.schedule(Tick(2), Phase::Transfer, 2).unwrap();
        assert_eq!(drain(&mut s), [0, 1, 2]);
    }

    #[test]
    fn s6_rejects_sequence_overflow_without_side_effects() {
        let snap = SchedulerSnapshot {
            last_dispatched: None,
            dispatched_in_phase: 0,
            next_sequence: u64::MAX - 1,
            queue: Vec::new(),
        };
        let mut s: Scheduler<u32> = Scheduler::restore(SchedulerConfig::default(), snap).unwrap();
        let key = s.schedule(Tick(0), Phase::Request, 1).unwrap();
        assert_eq!(key.sequence, u64::MAX - 1);
        assert_eq!(
            s.schedule(Tick(0), Phase::Request, 2),
            Err(SimError::SequenceOverflow)
        );
        assert_eq!(s.len(), 1);
        assert_eq!(s.next_sequence(), u64::MAX);
    }

    #[test]
    fn snapshot_round_trips_and_resumes() {
        let mut s = sched();
        for (t, p) in [(5, Phase::Commit), (1, Phase::Request), (5, Phase::Request)] {
            s.schedule(Tick(t), p, t as u32).unwrap();
        }
        s.pop().unwrap();
        let snap = s.snapshot();
        assert!(snap.queue.windows(2).all(|w| w[0].key < w[1].key));

        let mut shuffled = snap.clone();
        shuffled.queue.reverse();
        let mut r = Scheduler::restore(SchedulerConfig::default(), shuffled).unwrap();
        assert_eq!(r.snapshot(), snap);
        assert_eq!(drain(&mut r), drain(&mut s));
    }

    #[test]
    fn restore_rejects_impossible_states() {
        let key = |tick, phase, sequence| EventKey {
            tick: Tick(tick),
            phase,
            sequence,
        };
        let ev = |k| Event {
            key: k,
            payload: 0u32,
        };
        let base = SchedulerSnapshot {
            last_dispatched: Some(key(5, Phase::Complete, 3)),
            dispatched_in_phase: 1,
            next_sequence: 10,
            queue: vec![ev(key(5, Phase::Complete, 4))],
        };
        let restore = |s: SchedulerSnapshot<u32>| {
            Scheduler::restore(SchedulerConfig::default(), s).map(|_| ())
        };
        assert_eq!(restore(base.clone()), Ok(()));

        let mut dup = base.clone();
        dup.queue.push(ev(key(6, Phase::Request, 4)));
        assert_eq!(
            restore(dup),
            Err(SchedulerRestoreError::DuplicateSequence(4))
        );

        let mut ahead = base.clone();
        ahead.queue.push(ev(key(6, Phase::Request, 10)));
        assert_eq!(
            restore(ahead),
            Err(SchedulerRestoreError::SequenceNotYetAssigned(10))
        );

        let mut behind = base.clone();
        behind.queue.push(ev(key(5, Phase::Transfer, 5)));
        assert!(matches!(
            restore(behind),
            Err(SchedulerRestoreError::EventBeforeCursor(_))
        ));

        let mut observe = base.clone();
        observe.queue.push(ev(key(6, Phase::Observe, 5)));
        assert!(matches!(
            restore(observe),
            Err(SchedulerRestoreError::ObservePhaseEvent(_))
        ));

        let mut zero = base.clone();
        zero.dispatched_in_phase = 0;
        assert_eq!(
            restore(zero),
            Err(SchedulerRestoreError::InvalidPhaseCount(0))
        );

        let fresh = SchedulerSnapshot::<u32> {
            last_dispatched: None,
            dispatched_in_phase: 1,
            next_sequence: 0,
            queue: Vec::new(),
        };
        assert_eq!(
            restore(fresh),
            Err(SchedulerRestoreError::InvalidPhaseCount(1))
        );
    }
}
