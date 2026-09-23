//! Property tests for the scheduler (`docs/m0-design.md` §4.3, supporting tests).
//!
//! The scheduler is compared step by step against a deliberately naive reference model:
//! pending events live in a `Vec`, the next event is found by a linear scan over plain
//! `(tick, phase, sequence)` tuples, and every rule is written out independently. Any
//! difference in dispatch order, assigned keys, or errors fails the test.

use proptest::prelude::*;
use systemscope_contracts::error::SimError;
use systemscope_contracts::event::{EventKey, Phase};
use systemscope_contracts::time::Tick;
use systemscope_runtime::scheduler::{Event, Scheduler, SchedulerConfig, SchedulerSnapshot};

#[derive(Clone, Debug)]
enum Op {
    /// Schedule at `now + dt` (saturating at 0) in `Phase::ALL[phase]`.
    Schedule {
        dt: i8,
        phase: usize,
    },
    Pop,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (-1i8..=3, 0usize..5).prop_map(|(dt, phase)| Op::Schedule { dt, phase }),
        2 => Just(Op::Pop),
    ]
}

/// Reference model of rules S1–S6 with no heap and no `EventKey` comparator.
struct Model {
    limit: u64,
    last: Option<(u64, u8, u64)>,
    count: u64,
    next_seq: u64,
    pending: Vec<(u64, u8, u64, u32)>,
}

fn phase_of(rank: u8) -> Phase {
    Phase::ALL[usize::from(rank)]
}

fn key_of((tick, rank, seq): (u64, u8, u64)) -> EventKey {
    EventKey {
        tick: Tick(tick),
        phase: phase_of(rank),
        sequence: seq,
    }
}

impl Model {
    fn now(&self) -> (u64, u8) {
        self.last.map_or((0, 0), |(t, p, _)| (t, p))
    }

    fn schedule(&mut self, tick: u64, rank: u8, payload: u32) -> Result<EventKey, SimError> {
        let (now, cur) = self.now();
        let violation = SimError::PhaseViolation {
            now: Tick(now),
            current: phase_of(cur),
            tick: Tick(tick),
            requested: phase_of(rank),
        };
        if rank == 4 {
            return Err(violation);
        }
        if tick < now {
            return Err(SimError::PastTick {
                now: Tick(now),
                requested: Tick(tick),
            });
        }
        if tick == now && rank < cur {
            return Err(violation);
        }
        if self.next_seq == u64::MAX {
            return Err(SimError::SequenceOverflow);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push((tick, rank, seq, payload));
        Ok(key_of((tick, rank, seq)))
    }

    fn pop(&mut self) -> Result<Option<(EventKey, u32)>, SimError> {
        let Some(i) = (0..self.pending.len()).min_by_key(|&i| {
            let (t, p, s, _) = self.pending[i];
            (t, p, s)
        }) else {
            return Ok(None);
        };
        let (t, p, s, payload) = self.pending[i];
        let count = match self.last {
            Some((lt, lp, _)) if lt == t && lp == p => self.count + 1,
            _ => 1,
        };
        if count > self.limit {
            return Err(SimError::SameTickLivelock {
                tick: Tick(t),
                phase: phase_of(p),
                limit: self.limit,
            });
        }
        self.pending.swap_remove(i);
        self.last = Some((t, p, s));
        self.count = count;
        Ok(Some((key_of((t, p, s)), payload)))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Scheduled(Result<EventKey, SimError>),
    Popped(Result<Option<(EventKey, u32)>, SimError>),
}

fn scheduler_at(start_seq: u64, limit: u64) -> Scheduler<u32> {
    let snapshot = SchedulerSnapshot {
        last_dispatched: None,
        dispatched_in_phase: 0,
        next_sequence: start_seq,
        queue: Vec::new(),
    };
    Scheduler::restore(config(limit), snapshot).unwrap()
}

fn config(limit: u64) -> SchedulerConfig {
    SchedulerConfig {
        max_events_per_phase: limit,
    }
}

fn apply(s: &mut Scheduler<u32>, op: &Op, payload: u32) -> Outcome {
    match *op {
        Op::Schedule { dt, phase } => {
            let tick = s.now().0.saturating_add_signed(i64::from(dt));
            Outcome::Scheduled(s.schedule(Tick(tick), Phase::ALL[phase], payload))
        }
        Op::Pop => Outcome::Popped(s.pop().map(|e| e.map(|e| (e.key, e.payload)))),
    }
}

fn apply_model(m: &mut Model, op: &Op, payload: u32) -> Outcome {
    match *op {
        Op::Schedule { dt, phase } => {
            let tick = m.now().0.saturating_add_signed(i64::from(dt));
            Outcome::Scheduled(m.schedule(tick, phase as u8, payload))
        }
        Op::Pop => Outcome::Popped(m.pop()),
    }
}

fn start_seq() -> impl Strategy<Value = u64> {
    prop_oneof![Just(0u64), (u64::MAX - 8)..=u64::MAX]
}

fn limit() -> impl Strategy<Value = u64> {
    prop_oneof![1u64..=4, Just(1_000_000)]
}

fn drain(s: &mut Scheduler<u32>) -> Vec<Result<Option<(EventKey, u32)>, SimError>> {
    let mut out = Vec::new();
    loop {
        let r = s.pop().map(|e| e.map(|e| (e.key, e.payload)));
        let stop = !matches!(r, Ok(Some(_)));
        out.push(r);
        if stop {
            return out;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn matches_reference_model(
        ops in prop::collection::vec(op(), 0..200),
        start in start_seq(),
        limit in limit(),
    ) {
        let mut s = scheduler_at(start, limit);
        let mut m = Model { limit, last: None, count: 0, next_seq: start, pending: Vec::new() };
        for (i, op) in ops.iter().enumerate() {
            prop_assert_eq!(apply(&mut s, op, i as u32), apply_model(&mut m, op, i as u32));
            prop_assert_eq!(s.next_sequence(), m.next_seq);
            prop_assert_eq!(s.len(), m.pending.len());
        }
        // Drain what remains; zero-delay leftovers must come out in the model's order too.
        loop {
            let (a, b) = (apply(&mut s, &Op::Pop, 0), apply_model(&mut m, &Op::Pop, 0));
            prop_assert_eq!(&a, &b);
            if !matches!(a, Outcome::Popped(Ok(Some(_)))) {
                break;
            }
        }
    }

    #[test]
    fn same_tick_and_phase_dispatches_in_insertion_order(
        tick in 0u64..=u64::MAX,
        phase in 0usize..4,
        n in 1usize..300,
    ) {
        let mut s: Scheduler<u32> = Scheduler::new(SchedulerConfig::default());
        for i in 0..n {
            s.schedule(Tick(tick), Phase::ALL[phase], i as u32).unwrap();
        }
        let order: Vec<u32> = std::iter::from_fn(|| s.pop().unwrap().map(|e| e.payload)).collect();
        prop_assert_eq!(order, (0..n as u32).collect::<Vec<_>>());
    }

    #[test]
    fn snapshot_restore_is_transparent(
        prefix in prop::collection::vec(op(), 0..150),
        suffix in prop::collection::vec(op(), 0..150),
        shuffle in prop::collection::vec(any::<u64>(), 150),
        start in start_seq(),
        limit in limit(),
    ) {
        let mut s = scheduler_at(start, limit);
        for (i, op) in prefix.iter().enumerate() {
            apply(&mut s, op, i as u32);
        }
        let snap = s.snapshot();
        prop_assert!(snap.queue.windows(2).all(|w| w[0].key < w[1].key));

        // Restore from an arbitrarily permuted queue: heap layout must not matter.
        let mut permuted = snap.clone();
        let mut tagged: Vec<(u64, Event<u32>)> =
            shuffle.iter().copied().zip(permuted.queue.drain(..)).collect();
        tagged.sort_by_key(|(tag, _)| *tag);
        permuted.queue = tagged.into_iter().map(|(_, e)| e).collect();
        let mut r = Scheduler::restore(config(limit), permuted).unwrap();
        prop_assert_eq!(r.snapshot(), snap.clone());

        for (i, op) in suffix.iter().enumerate() {
            let payload = (prefix.len() + i) as u32;
            prop_assert_eq!(apply(&mut s, op, payload), apply(&mut r, op, payload));
        }
        prop_assert_eq!(r.snapshot(), s.snapshot());
        prop_assert_eq!(drain(&mut r), drain(&mut s));
    }
}
