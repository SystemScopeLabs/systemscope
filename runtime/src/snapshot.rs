//! `RuntimeSnapshot`: encoding, strict decoding, and validated restore
//! (`docs/m0-design.md` §7).
//!
//! A snapshot is canonical bytes, and `StateDigest` is their BLAKE3. Restore decodes the
//! whole snapshot, checks it against the resuming session in a fixed order, and only then
//! hands each component its own bytes. Trace state is never part of a snapshot.

use systemscope_contracts::canonical::{CanonicalEvent, DecodeError, Decoder, Encoder};
use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::snapshot::{RestoreError, SessionField};
use systemscope_contracts::time::ClockDomain;
use systemscope_contracts::trace::{CONTRACTS_VERSION, encode_clock_domains};

use crate::rng::Xoshiro256StarStar;
use crate::runtime::{Lifecycle, Pending, Runtime, RuntimeError};
use crate::scheduler::{Event, Scheduler, SchedulerRestoreError, SchedulerSnapshot};

/// First bytes of every snapshot.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"SSSNAP\0\0";

/// Version of the snapshot layout.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// A snapshot decoded but not yet checked against a session.
struct Decoded<'a> {
    seed: u64,
    ticks_per_second: u64,
    max_events_per_phase: u64,
    contracts_version: &'a str,
    clock_domains: Vec<(u32, u64, u64, u64, u8)>,
    topology_hash: [u8; 32],
    last_dispatched: Option<EventKey>,
    dispatched_in_phase: u64,
    next_sequence: u64,
    execution_digest: [u8; 32],
    queue: Vec<CanonicalEvent>,
    rng_states: Vec<[u64; 4]>,
    /// `(id, schema_version, bytes)` per component.
    components: Vec<(u32, u32, &'a [u8])>,
}

fn decode(bytes: &[u8]) -> Result<Decoded<'_>, RestoreError> {
    let mut d = Decoder::new(bytes);
    if d.raw(SNAPSHOT_MAGIC.len()) != Ok(&SNAPSHOT_MAGIC[..]) {
        return Err(RestoreError::BadMagic);
    }
    let version = d.u32()?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(RestoreError::FormatVersion(version));
    }
    let seed = d.u64()?;
    let ticks_per_second = d.u64()?;
    let max_events_per_phase = d.u64()?;
    let contracts_version = d.str()?;
    let mut clock_domains = Vec::new();
    for _ in 0..d.len()? {
        let domain = (d.u32()?, d.u64()?, d.u64()?, d.u64()?, d.u8()?);
        if domain.4 > 1 {
            return Err(DecodeError::InvalidTag {
                what: "rounding",
                tag: domain.4,
            }
            .into());
        }
        clock_domains.push(domain);
    }
    let topology_hash = d.array()?;
    let last_dispatched = match d.u8()? {
        0 => None,
        1 => Some(EventKey::decode(&mut d)?),
        tag => {
            return Err(DecodeError::InvalidTag {
                what: "option",
                tag,
            }
            .into());
        }
    };
    let dispatched_in_phase = d.u64()?;
    let next_sequence = d.u64()?;
    let execution_digest = d.array()?;
    let mut queue = Vec::new();
    for _ in 0..d.len()? {
        queue.push(CanonicalEvent::decode(&mut d)?);
    }
    let mut rng_states = Vec::new();
    for _ in 0..d.len()? {
        rng_states.push([d.u64()?, d.u64()?, d.u64()?, d.u64()?]);
    }
    let mut components = Vec::new();
    for _ in 0..d.len()? {
        components.push((d.u32()?, d.u32()?, d.bytes()?));
    }
    d.finish()?;
    Ok(Decoded {
        seed,
        ticks_per_second,
        max_events_per_phase,
        contracts_version,
        clock_domains,
        topology_hash,
        last_dispatched,
        dispatched_in_phase,
        next_sequence,
        execution_digest,
        queue,
        rng_states,
        components,
    })
}

/// The decoded form of this session's clock domains, for comparison.
fn domain_fields(domains: &[ClockDomain]) -> Vec<(u32, u64, u64, u64, u8)> {
    let mut e = Encoder::new();
    encode_clock_domains(&mut e, domains);
    let bytes = e.into_bytes();
    let mut d = Decoder::new(&bytes);
    let n = d.len().expect("just encoded");
    (0..n)
        .map(|_| {
            let mut next = || -> Result<_, DecodeError> {
                Ok((d.u32()?, d.u64()?, d.u64()?, d.u64()?, d.u8()?))
            };
            next().expect("just encoded")
        })
        .collect()
}

fn scheduler_error(e: SchedulerRestoreError) -> RestoreError {
    RestoreError::InvalidState(match e {
        SchedulerRestoreError::DuplicateSequence(_) => "two queued events share a sequence",
        SchedulerRestoreError::SequenceNotYetAssigned(_) => {
            "a queued sequence is not below next_sequence"
        }
        SchedulerRestoreError::EventBeforeCursor(_) => {
            "a queued event is not after the last dispatched event"
        }
        SchedulerRestoreError::ObservePhaseEvent(_) => "a queued event is in OBSERVE",
        SchedulerRestoreError::InvalidPhaseCount(_) => "dispatched_in_phase is inconsistent",
    })
}

impl Runtime {
    /// Encodes the complete simulation state. Allowed only when `Ready`, at an event
    /// boundary. Tracing never changes the bytes.
    pub fn snapshot(&self) -> Result<Vec<u8>, RuntimeError> {
        self.require_state(Lifecycle::Ready)?;
        let mut e = Encoder::new();
        e.raw(&SNAPSHOT_MAGIC);
        e.u32(SNAPSHOT_FORMAT_VERSION);
        e.u64(self.config.seed);
        e.u64(self.clock.ticks_per_second());
        e.u64(self.config.scheduler.max_events_per_phase);
        e.str(CONTRACTS_VERSION);
        encode_clock_domains(&mut e, &self.domains);
        e.raw(&self.topology_hash);

        let scheduler = self.scheduler.snapshot();
        match scheduler.last_dispatched {
            None => e.u8(0),
            Some(key) => {
                e.u8(1);
                key.encode(&mut e);
            }
        }
        e.u64(scheduler.dispatched_in_phase);
        e.u64(scheduler.next_sequence);
        e.raw(&self.execution_digest);
        e.len(scheduler.queue.len());
        for event in scheduler.queue {
            let Pending {
                source,
                target,
                delivery,
            } = event.payload;
            CanonicalEvent {
                key: event.key,
                source,
                target,
                delivery,
            }
            .encode(&mut e);
        }
        e.len(self.rngs.len());
        for rng in &self.rngs {
            for word in rng.state() {
                e.u64(word);
            }
        }
        e.len(self.components.len());
        for (id, component) in (0u32..).zip(&self.components) {
            e.u32(id);
            e.u32(component.snapshot_schema_version());
            let mut w = Encoder::new();
            component.snapshot(&mut w);
            e.bytes(w.as_bytes());
        }
        Ok(e.into_bytes())
    }

    /// `StateDigest`: BLAKE3 of [`Runtime::snapshot`].
    pub fn state_digest(&self) -> Result<[u8; 32], RuntimeError> {
        self.snapshot()
            .map(|bytes| *blake3::hash(&bytes).as_bytes())
    }

    /// Replaces `init`: resumes the session a snapshot was taken from. Allowed only when
    /// `Elaborated` and before `start_trace`; continue a trace with `resume_trace`.
    ///
    /// Checks run in the order of §7 and the first failure is returned. Any failure faults
    /// the session, since components may already hold part of the snapshot.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), RuntimeError> {
        self.require_state(Lifecycle::Elaborated)?;
        if self.trace.is_some() {
            return Err(RuntimeError::TraceNeedsPrefix);
        }
        match self.try_restore(bytes) {
            Ok(()) => {
                self.lifecycle = Lifecycle::Ready;
                self.freshly_restored = true;
                Ok(())
            }
            Err(e) => Err(self.fail_restore(e)),
        }
    }

    fn try_restore(&mut self, bytes: &[u8]) -> Result<(), RestoreError> {
        let snap = decode(bytes)?;

        let session = [
            (snap.seed == self.config.seed, SessionField::Seed),
            (
                snap.ticks_per_second == self.clock.ticks_per_second(),
                SessionField::TicksPerSecond,
            ),
            (
                snap.max_events_per_phase == self.config.scheduler.max_events_per_phase,
                SessionField::MaxEventsPerPhase,
            ),
            (
                snap.contracts_version == CONTRACTS_VERSION,
                SessionField::ContractsVersion,
            ),
            (
                snap.clock_domains == domain_fields(&self.domains),
                SessionField::ClockDomains,
            ),
        ];
        if let Some((_, field)) = session.iter().find(|(same, _)| !same) {
            return Err(RestoreError::SessionMismatch(*field));
        }
        if snap.topology_hash != self.topology_hash {
            return Err(RestoreError::TopologyMismatch);
        }
        let entries = self.components.iter().zip(&snap.components);
        for (id, (component, &(_, found, _))) in (0u32..).zip(entries) {
            let expected = component.snapshot_schema_version();
            if found != expected {
                return Err(RestoreError::SchemaVersion {
                    component: ComponentId(id),
                    expected,
                    found,
                });
            }
        }

        let n = self.components.len();
        if snap.components.len() != n || snap.rng_states.len() != n {
            return Err(RestoreError::InvalidState("not one entry per component"));
        }
        if snap.components.iter().zip(0u32..).any(|(c, id)| c.0 != id) {
            return Err(RestoreError::InvalidState(
                "component entries out of id order",
            ));
        }
        let rngs = snap
            .rng_states
            .iter()
            .map(|&s| Xoshiro256StarStar::from_state(s))
            .collect::<Option<Vec<_>>>()
            .ok_or(RestoreError::InvalidState("all-zero RNG state"))?;
        if snap.last_dispatched.is_none() && snap.execution_digest != [0; 32] {
            return Err(RestoreError::InvalidState(
                "execution digest without a dispatched event",
            ));
        }
        let mut queue = Vec::with_capacity(snap.queue.len());
        for ev in snap.queue {
            self.check_event(&ev)?;
            let CanonicalEvent {
                key,
                source,
                target,
                delivery,
            } = ev;
            queue.push(Event {
                key,
                payload: Pending {
                    source,
                    target,
                    delivery,
                },
            });
        }
        let scheduler = Scheduler::restore(
            self.config.scheduler,
            SchedulerSnapshot {
                last_dispatched: snap.last_dispatched,
                dispatched_in_phase: snap.dispatched_in_phase,
                next_sequence: snap.next_sequence,
                queue,
            },
        )
        .map_err(scheduler_error)?;

        for (component, &(_, schema, bytes)) in self.components.iter_mut().zip(&snap.components) {
            let mut r = Decoder::new(bytes);
            component.restore(&mut r, schema)?;
            r.finish()?;
        }
        self.scheduler = scheduler;
        self.rngs = rngs;
        self.execution_digest = snap.execution_digest;
        Ok(())
    }

    /// A queued event must be one this topology could have scheduled.
    fn check_event(&self, ev: &CanonicalEvent) -> Result<(), RestoreError> {
        let target = self
            .slots
            .get(ev.target.0 as usize)
            .ok_or(RestoreError::InvalidState(
                "queued event for an unknown component",
            ))?;
        match &ev.delivery {
            Delivered::Message { port, msg } => {
                if ev.source != ComponentId::RUNTIME && ev.source.0 as usize >= self.slots.len() {
                    return Err(RestoreError::InvalidState(
                        "queued message from an unknown component",
                    ));
                }
                let spec =
                    target
                        .ports
                        .get(usize::from(port.0))
                        .ok_or(RestoreError::InvalidState(
                            "queued message for an unknown port",
                        ))?;
                if spec.protocol != msg.protocol() {
                    return Err(RestoreError::InvalidState(
                        "queued message does not match its port's protocol",
                    ));
                }
            }
            Delivered::Wake { .. } => {
                if ev.source != ev.target {
                    return Err(RestoreError::InvalidState(
                        "queued wake for another component",
                    ));
                }
            }
        }
        Ok(())
    }
}
