//! Where each field sits in a `RuntimeSnapshot` (`docs/m0-design.md` §7), so tests can
//! read the scheduler and RNG state and doctor single fields.

use std::ops::Range;

use systemscope_contracts::canonical::{CanonicalEvent, DecodeError, Decoder};
use systemscope_contracts::event::EventKey;

/// One component's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Offset of its `u32` id.
    pub id: usize,
    /// Offset of its `u32` snapshot schema version.
    pub schema: usize,
    /// Its own bytes.
    pub state: Range<usize>,
}

/// Field offsets, plus the values the acceptance tests compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Offset of the `u32` format version.
    pub format_version: usize,
    /// Offset of the `u64` seed.
    pub seed: usize,
    /// Offset of the `u64` ticks per second.
    pub ticks_per_second: usize,
    /// Offset of the `u64` S5 limit.
    pub max_events_per_phase: usize,
    /// Offset of the contracts version's length prefix.
    pub contracts_version: usize,
    /// Offset of the first clock domain's `u64` numerator.
    pub first_domain_num: usize,
    /// Offset of the topology hash.
    pub topology_hash: usize,
    /// Offset of the last dispatched event's option tag.
    pub last_dispatched: usize,
    /// Offset of the `u64` events dispatched in the current `(tick, phase)`.
    pub dispatched_in_phase: usize,
    /// Offset of the `u64` next sequence.
    pub next_sequence: usize,
    /// Offset of the execution digest.
    pub execution_digest: usize,
    /// Offset of each queued event.
    pub queue: Vec<usize>,
    /// Offset of each component's RNG state.
    pub rng: Vec<usize>,
    /// Each component's entry.
    pub components: Vec<Entry>,
    /// The next sequence number.
    pub next_sequence_value: u64,
    /// Each component's RNG state.
    pub rng_states: Vec<[u64; 4]>,
}

impl Layout {
    /// Walks a snapshot in the order the runtime writes it.
    pub fn parse(bytes: &[u8]) -> Result<Layout, DecodeError> {
        let mut d = Decoder::new(bytes);
        let at = |d: &Decoder<'_>| bytes.len() - d.remaining();
        d.raw(8)?;
        let format_version = at(&d);
        d.u32()?;
        let seed = at(&d);
        d.u64()?;
        let ticks_per_second = at(&d);
        d.u64()?;
        let max_events_per_phase = at(&d);
        d.u64()?;
        let contracts_version = at(&d);
        d.str()?;
        let mut first_domain_num = 0;
        for i in 0..d.len()? {
            d.u32()?;
            if i == 0 {
                first_domain_num = at(&d);
            }
            d.raw(3 * 8 + 1)?;
        }
        let topology_hash = at(&d);
        d.raw(32)?;
        let last_dispatched = at(&d);
        if d.u8()? == 1 {
            EventKey::decode(&mut d)?;
        }
        let dispatched_in_phase = at(&d);
        d.u64()?;
        let next_sequence = at(&d);
        let next_sequence_value = d.u64()?;
        let execution_digest = at(&d);
        d.raw(32)?;
        let mut queue = Vec::new();
        for _ in 0..d.len()? {
            queue.push(at(&d));
            CanonicalEvent::decode(&mut d)?;
        }
        let mut rng = Vec::new();
        let mut rng_states = Vec::new();
        for _ in 0..d.len()? {
            rng.push(at(&d));
            rng_states.push([d.u64()?, d.u64()?, d.u64()?, d.u64()?]);
        }
        let mut components = Vec::new();
        for _ in 0..d.len()? {
            let id = at(&d);
            d.u32()?;
            let schema = at(&d);
            d.u32()?;
            let len = d.u32()? as usize;
            let start = at(&d);
            d.raw(len)?;
            components.push(Entry {
                id,
                schema,
                state: start..start + len,
            });
        }
        d.finish()?;
        Ok(Layout {
            format_version,
            seed,
            ticks_per_second,
            max_events_per_phase,
            contracts_version,
            first_domain_num,
            topology_hash,
            last_dispatched,
            dispatched_in_phase,
            next_sequence,
            execution_digest,
            queue,
            rng,
            components,
            next_sequence_value,
            rng_states,
        })
    }
}

/// Overwrites the little-endian `u64` at `at`.
pub fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Overwrites the little-endian `u32` at `at`.
pub fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Reads the little-endian `u64` at `at`.
pub fn get_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}
