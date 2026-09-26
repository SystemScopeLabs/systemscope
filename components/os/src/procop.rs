//! The process-mode pure core (`docs/m3-design.md` §6.3, §6.6, §6.7, §8.3): the kernel
//! operations of a kernel with a [`ProcessPlan`], over the same abstract memory interface
//! as [`crate::core`], with no runtime.
//!
//! # Operations
//!
//! An `ENTER` of the trap frame address starts `Boot` on the first entry and `Trap` on
//! every later one (§6.3); any other value starts the failure shutdown, as in M3.4a. An
//! operation is a chain of **stages**, each a fixed list of accesses:
//!
//! ```text
//! Boot:  Create(pid 1) → Create(pid 2) → … → Dispatch(head) | Shutdown
//! Trap:  ReadFrame → Dispatch(head) | Shutdown
//! ```
//!
//! - **Create** builds one process's address space ([`Space`]): zero, copy, map. Its
//!   frames are reserved when the stage starts, all at once, so pool exhaustion skips the
//!   image with an `os.process.create` error before anything is written, and boot
//!   continues (§6.4). When its last PTE is written, the process is admitted `Ready` at
//!   the queue's tail with its initial context.
//! - **ReadFrame** reads the whole trap frame. Then the kernel decides (§6.6, §6.7):
//!   - `SPP = S`: shut down with reason 1;
//!   - `scause = 8` (`ecall` from U): **the M3.4b cooperative switch point**. The frame
//!     becomes the process's saved context with `sepc + 4`, the process goes to the
//!     queue's tail, and the head is dispatched. No register is decoded: the syscall ABI
//!     (§6.5) is M3.5's, which will route `sched_yield` to the same
//!     [`Processes::yield_current`] and give the other numbers their meaning;
//!   - any other cause: the process becomes `Faulted { scause, sepc, stval }`, its
//!     frames are freed, and the head is dispatched.
//! - **Dispatch** writes the head's context (`x1`–`x31`, `sepc`, `sstatus`), then its
//!   `satp` and `action = Resume`, into the trap frame. The trampoline does the rest.
//! - **Shutdown** writes `action = Shutdown` and the reason. With an empty queue the
//!   reason is 0 only when every boot image became a process and every process exited
//!   with status 0 (§6.6); in M3.4b no process can exit, so it is 1.
//!
//! Every metadata change happens at a stage boundary, in the same step as the completion
//! that ends the stage; within a stage, the only kernel state that moves is the step and
//! the working data.
//!
//! # Working data
//!
//! A Create holds the bytes of the copy between its read and its write; a ReadFrame the
//! frame bytes read so far; a Dispatch the context it is writing, which has left the PCB
//! (the process is `Running`) and has not all reached the frame yet. All of it is dropped
//! when the operation ends (§6.8).

use systemscope_contracts::trace::Value;

use crate::config::{FRAME_ACTION, FRAME_BYTES, KernelConfig};
use crate::core::{Access, Completion, CoreError, chunks};
use crate::image::{ProcessPlan, perm_str};
use crate::process::{CONTEXT_BYTES, Context, Pcb, ProcState, Processes, Violation};
use crate::pte;
use crate::space::{Space, frames_needed, megapage_conflict};

/// The offset of `sstatus` in the trap frame.
pub const FRAME_SSTATUS: u64 = 0x80;
/// The offset of `scause` in the trap frame.
pub const FRAME_SCAUSE: u64 = 0x84;
/// The offset of `stval` in the trap frame.
pub const FRAME_STVAL: u64 = 0x88;
/// The offset of `satp` in the trap frame.
pub const FRAME_SATP: u64 = 0x8C;
/// `sstatus.SPP`.
pub const SSTATUS_SPP: u32 = 1 << 8;
/// `scause` of an `ecall` from U-mode.
pub const ECALL_FROM_U: u32 = 8;

/// Trace kind of the start of `Boot` (§6.8): `entries`.
pub const BOOT_KIND: &str = "os.boot";
/// Trace kind of a creation (§6.8): `pid`, `entry`, `root`, `error` (empty on success).
pub const CREATE_KIND: &str = "os.process.create";
/// Trace kind of a mapped segment (§6.8): `pid`, `va`, `memsz`, `perms`.
pub const SEGMENT_KIND: &str = "os.load.segment";
/// Trace kind of a dispatch (§6.8): `from` (0 for none), `to`.
pub const SWITCH_KIND: &str = "os.process.switch";
/// Trace kind of a fault kill (§6.8): `pid`, `cause`, `epc`, `tval`.
pub const FAULT_KIND: &str = "os.process.fault";

/// Where a kernel with processes is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Life {
    /// No `ENTER` yet.
    AwaitBoot,
    /// Booted or booting.
    Up,
    /// A shutdown has been decided.
    Down,
}

/// A trace record an operation asks the component to emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// The trace kind.
    pub kind: &'static str,
    /// The fields.
    pub fields: Vec<(&'static str, Value)>,
}

/// One stage of a process-mode operation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Building process `pid` in `space`, whose frames are reserved for it.
    Create {
        /// The PID being created.
        pid: u32,
        /// Its address space.
        space: Space,
    },
    /// Reading the trap frame.
    ReadFrame,
    /// Writing `context` and `satp` for `pid`, which is `Running`.
    Dispatch {
        /// The PID being dispatched.
        pid: u32,
        /// Its context, on its way to the frame.
        context: Context,
        /// Its `satp`.
        satp: u32,
    },
    /// Writing `action = Shutdown` and `reason`.
    Shutdown {
        /// 0 or 1 (§7.4).
        reason: u32,
    },
}

impl Stage {
    /// The name inspect uses.
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Create { .. } => "create",
            Stage::ReadFrame => "read_frame",
            Stage::Dispatch { .. } => "dispatch",
            Stage::Shutdown { .. } => "shutdown",
        }
    }
}

/// A running process-mode operation: its stage, the accesses of the stage completed, and
/// the working data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcOp {
    stage: Stage,
    step: u32,
    data: Vec<u8>,
}

fn frame_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    chunks(u64::from(config.trap_frame), FRAME_BYTES)
}

fn dispatch_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    let base = u64::from(config.trap_frame);
    let mut out = chunks(base, CONTEXT_BYTES as u64);
    out.extend(chunks(base + FRAME_SATP, 8));
    out
}

fn shutdown_chunks(config: &KernelConfig) -> Vec<(u64, u64)> {
    chunks(u64::from(config.trap_frame) + FRAME_ACTION, 8)
}

fn word(frame: &[u8], offset: u64) -> u32 {
    let at = offset as usize;
    u32::from_le_bytes(frame[at..at + 4].try_into().expect("4 bytes"))
}

/// `scause`'s name, spelled as the CPU's `rv32.trap` records spell exceptions.
pub fn cause_name(scause: u32) -> String {
    let name = match scause {
        0 => "InstructionAddressMisaligned",
        1 => "InstructionAccessFault",
        2 => "IllegalInstruction",
        3 => "Breakpoint",
        4 => "LoadAddressMisaligned",
        5 => "LoadAccessFault",
        6 => "StoreAddressMisaligned",
        7 => "StoreAccessFault",
        8 => "EnvironmentCallFromU",
        9 => "EnvironmentCallFromS",
        12 => "InstructionPageFault",
        13 => "LoadPageFault",
        15 => "StorePageFault",
        other => return format!("scause {other:#x}"),
    };
    name.to_owned()
}

impl ProcOp {
    /// An operation at the start of `stage`.
    pub fn new(stage: Stage) -> ProcOp {
        ProcOp {
            stage,
            step: 0,
            data: Vec::new(),
        }
    }

    /// An operation at `step` of `stage` with `data`, or `None` if no run reaches it: a
    /// step at or past the stage's last access, or working data of another length than
    /// the step requires.
    pub fn from_parts(
        config: &KernelConfig,
        stage: Stage,
        step: u32,
        data: Vec<u8>,
    ) -> Option<ProcOp> {
        let op = ProcOp { stage, step, data };
        (step < op.steps(config) && op.data.len() == op.data_len(config, step)).then_some(op)
    }

    /// The stage.
    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    /// The number of the stage's accesses completed.
    pub fn step(&self) -> u32 {
        self.step
    }

    /// The working data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The number of accesses of the stage.
    pub fn steps(&self, config: &KernelConfig) -> u32 {
        let n = match &self.stage {
            Stage::Create { space, .. } => return space.steps(),
            Stage::ReadFrame => frame_chunks(config).len(),
            Stage::Dispatch { .. } => dispatch_chunks(config).len(),
            Stage::Shutdown { .. } => shutdown_chunks(config).len(),
        };
        u32::try_from(n).expect("a handful of accesses")
    }

    /// The working data length at `step`.
    fn data_len(&self, config: &KernelConfig, step: u32) -> usize {
        match &self.stage {
            Stage::Create { space, .. } => space.data_len(step),
            Stage::ReadFrame => frame_chunks(config)[..step as usize]
                .iter()
                .map(|c| c.1 as usize)
                .sum(),
            Stage::Dispatch { .. } | Stage::Shutdown { .. } => 0,
        }
    }

    /// The access due at the current step, or `None` once the stage has finished.
    pub fn access(&self, config: &KernelConfig) -> Option<Access> {
        let step = self.step as usize;
        let written = |chunks: Vec<(u64, u64)>, base: u64, bytes: &[u8]| {
            let (addr, len) = *chunks.get(step)?;
            let at = (addr - base) as usize;
            Some(Access::Write {
                addr,
                data: bytes[at..at + len as usize].to_vec(),
            })
        };
        let frame = u64::from(config.trap_frame);
        match &self.stage {
            Stage::Create { space, .. } => space.access(self.step, &self.data),
            Stage::ReadFrame => {
                let (addr, len) = *frame_chunks(config).get(step)?;
                Some(Access::Read {
                    addr,
                    len: len as u32,
                })
            }
            Stage::Dispatch { context, satp, .. } => {
                let mut bytes = vec![0; FRAME_BYTES as usize];
                bytes[..CONTEXT_BYTES].copy_from_slice(&context.to_frame());
                let at = FRAME_SATP as usize;
                bytes[at..at + 4].copy_from_slice(&satp.to_le_bytes());
                // action = Resume (0) at FRAME_ACTION is already zero.
                written(dispatch_chunks(config), frame, &bytes)
            }
            Stage::Shutdown { reason } => {
                let mut bytes = [0u8; 8];
                bytes[..4].copy_from_slice(&1u32.to_le_bytes());
                bytes[4..].copy_from_slice(&reason.to_le_bytes());
                written(shutdown_chunks(config), frame + FRAME_ACTION, &bytes)
            }
        }
    }

    /// Advances past the current access, which `completion` answers. Changes nothing on
    /// an error.
    pub fn complete(
        &mut self,
        config: &KernelConfig,
        completion: Completion,
    ) -> Result<(), CoreError> {
        let access = self.access(config).ok_or(CoreError::Finished)?;
        match (access, completion) {
            (Access::Read { len, .. }, Completion::Data(data)) => {
                if data.len() != len as usize {
                    return Err(CoreError::DataLength);
                }
                self.data.extend_from_slice(&data);
            }
            (Access::Write { .. }, Completion::Written) => {}
            _ => return Err(CoreError::WrongKind),
        }
        self.step += 1;
        if self.step < self.steps(config) && self.data.len() != self.data_len(config, self.step) {
            // A copy's write drops the bytes it wrote.
            self.data.clear();
        }
        Ok(())
    }

    /// Whether every access of the stage has completed.
    pub fn is_finished(&self, config: &KernelConfig) -> bool {
        self.step >= self.steps(config)
    }
}

/// The process-mode kernel metadata: the plan, the processes, and where the kernel is in
/// its life.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Model {
    /// The plan.
    pub plan: ProcessPlan,
    /// The processes and frames.
    pub procs: Processes,
    /// Where the kernel is in its life.
    pub life: Life,
}

fn note(kind: &'static str, fields: Vec<(&'static str, Value)>) -> Note {
    Note { kind, fields }
}

fn shutdown_note(reason: u32, detail: String) -> Note {
    note(
        crate::kernel::SHUTDOWN_KIND,
        vec![
            ("reason", Value::U64(u64::from(reason))),
            ("detail", Value::Str(detail)),
        ],
    )
}

fn create_error(pid: u32, entry: u32, error: &str) -> Note {
    note(
        CREATE_KIND,
        vec![
            ("pid", Value::U64(u64::from(pid))),
            ("entry", Value::U64(u64::from(entry))),
            ("root", Value::U64(0)),
            ("error", Value::Str(error.to_owned())),
        ],
    )
}

impl Model {
    /// The name of the operation an `ENTER` of `value` starts now: `boot`, `trap`, or
    /// `shutdown`.
    pub fn op_name(&self, config: &KernelConfig, value: u32) -> &'static str {
        match self.life {
            _ if value != config.trap_frame => "shutdown",
            Life::AwaitBoot => "boot",
            Life::Up | Life::Down => "trap",
        }
    }

    /// Starts the operation of an `ENTER` of `value` (§6.3). A kernel bug, such as an
    /// entry after the shutdown or with no process running, is a [`Violation`].
    pub fn start(
        &mut self,
        config: &KernelConfig,
        value: u32,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        if value != config.trap_frame {
            if self.life == Life::Down {
                return Err(Violation("ENTER after the shutdown"));
            }
            self.life = Life::Down;
            notes.push(shutdown_note(
                1,
                "ENTER value is not the trap frame".to_owned(),
            ));
            return Ok(ProcOp::new(Stage::Shutdown { reason: 1 }));
        }
        match self.life {
            Life::AwaitBoot => {
                self.life = Life::Up;
                notes.push(note(
                    BOOT_KIND,
                    vec![("entries", Value::U64(self.plan.images.len() as u64))],
                ));
                self.next_creation(config, 0, notes)
            }
            Life::Up if self.procs.current().is_some() => Ok(ProcOp::new(Stage::ReadFrame)),
            Life::Up => Err(Violation("ENTER with no process running")),
            Life::Down => Err(Violation("ENTER after the shutdown")),
        }
    }

    /// Moves past the finished stage of `op`: the next stage, or `None` when the
    /// operation has finished.
    pub fn advance(
        &mut self,
        config: &KernelConfig,
        op: ProcOp,
        notes: &mut Vec<Note>,
    ) -> Result<Option<ProcOp>, Violation> {
        match op.stage {
            Stage::Create { pid, space } => {
                let boot = &self.plan.images[pid as usize - 1];
                let entry = boot.image.entry;
                let context = Context::initial(entry, self.plan.layout.stack_top);
                self.procs.admit(Pcb::new(pid, &space, context))?;
                for s in &boot.image.segments {
                    notes.push(note(
                        SEGMENT_KIND,
                        vec![
                            ("pid", Value::U64(u64::from(pid))),
                            ("va", Value::U64(u64::from(s.vaddr))),
                            ("memsz", Value::U64(u64::from(s.memsz))),
                            ("perms", Value::Str(perm_str(s.perms))),
                        ],
                    ));
                }
                notes.push(note(
                    CREATE_KIND,
                    vec![
                        ("pid", Value::U64(u64::from(pid))),
                        ("entry", Value::U64(u64::from(entry))),
                        ("root", Value::U64(u64::from(space.root))),
                        ("error", Value::Str(String::new())),
                    ],
                ));
                self.next_creation(config, pid as usize, notes).map(Some)
            }
            Stage::ReadFrame => self.decide(&op.data, notes).map(Some),
            Stage::Dispatch { .. } | Stage::Shutdown { .. } => Ok(None),
        }
    }

    /// The first image from `from` on that can be created, reserving its frames; or, when
    /// none is left, the first dispatch.
    fn next_creation(
        &mut self,
        config: &KernelConfig,
        from: usize,
        notes: &mut Vec<Note>,
    ) -> Result<ProcOp, Violation> {
        for index in from..self.plan.images.len() {
            let pid = index as u32 + 1;
            let boot = &self.plan.images[index];
            if megapage_conflict(&boot.image, config) {
                notes.push(create_error(
                    pid,
                    boot.image.entry,
                    "a segment is in a megapage slot",
                ));
                continue;
            }
            let n = frames_needed(&boot.image, &self.plan.layout);
            let Some(frames) = self.procs.frames_mut().reserve(pid, n) else {
                notes.push(create_error(pid, boot.image.entry, "frame pool exhausted"));
                continue;
            };
            let space = Space::build(config, &self.plan.layout, boot, &frames);
            return Ok(ProcOp::new(Stage::Create { pid, space }));
        }
        self.dispatch_next(0, notes)
    }

    /// Dispatches the queue's head, or shuts down when the queue is empty (§6.6).
    fn dispatch_next(&mut self, from: u32, notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        match self.procs.schedule_next()? {
            Some((pid, context)) => {
                let root = self.procs.pcb(pid).expect("scheduled").root;
                notes.push(note(
                    SWITCH_KIND,
                    vec![
                        ("from", Value::U64(u64::from(from))),
                        ("to", Value::U64(u64::from(pid))),
                    ],
                ));
                Ok(ProcOp::new(Stage::Dispatch {
                    pid,
                    context,
                    satp: pte::satp(root),
                }))
            }
            None => {
                let reason = self.empty_queue_reason();
                self.life = Life::Down;
                notes.push(shutdown_note(reason, "the run queue is empty".to_owned()));
                Ok(ProcOp::new(Stage::Shutdown { reason }))
            }
        }
    }

    /// The shutdown reason for an empty queue: 0 only when every image became a process
    /// and every process exited with status 0.
    pub fn empty_queue_reason(&self) -> u32 {
        let pcbs = self.procs.pcbs();
        let clean = pcbs.len() == self.plan.images.len()
            && pcbs
                .iter()
                .all(|p| p.state == ProcState::Exited { status: 0 });
        u32::from(!clean)
    }

    /// The decision after a trap (§6.6, §6.7), from the frame bytes read.
    fn decide(&mut self, frame: &[u8], notes: &mut Vec<Note>) -> Result<ProcOp, Violation> {
        let sstatus = word(frame, FRAME_SSTATUS);
        let scause = word(frame, FRAME_SCAUSE);
        let sepc = word(frame, crate::config::FRAME_SEPC);
        let stval = word(frame, FRAME_STVAL);
        if sstatus & SSTATUS_SPP != 0 {
            self.life = Life::Down;
            notes.push(shutdown_note(
                1,
                format!("trap from S-mode: {}", cause_name(scause)),
            ));
            return Ok(ProcOp::new(Stage::Shutdown { reason: 1 }));
        }
        if scause == ECALL_FROM_U {
            let mut context = Context::from_frame(frame);
            context.pc = sepc.wrapping_add(4);
            let pid = self.procs.yield_current(context)?;
            return self.dispatch_next(pid, notes);
        }
        let pid = self.procs.end_current(ProcState::Faulted {
            cause: scause,
            epc: sepc,
            tval: stval,
        })?;
        notes.push(note(
            FAULT_KIND,
            vec![
                ("pid", Value::U64(u64::from(pid))),
                ("cause", Value::Str(cause_name(scause))),
                ("epc", Value::U64(u64::from(sepc))),
                ("tval", Value::U64(u64::from(stval))),
            ],
        ));
        self.dispatch_next(pid, notes)
    }
}
