//! An independent reader of `m3-reference` runtime snapshots, and the platform-level
//! checks that no single component's restore can make (`docs/m3-design.md` §9, §17.4).
//!
//! [`Platform::decode`] walks the bytes from the documented layouts alone: the runtime
//! container (`docs/m0-design.md` §7), then each component's schema as its `snapshot`
//! documentation states it (the CPU's schema 3 of §5.5, the kernel's schema 1 of §6.8,
//! and the unchanged schema 1 of the RAM, the bus, the UART, the IRQ controller, the
//! block controller, and the media). It shares no code with the components, so a layout
//! that drifts from its documentation fails here even when the component still reads its
//! own bytes back. It decodes everything before it checks anything, and it never panics
//! on any input.
//!
//! [`Platform::check`] then validates what spans components, on the decoded values
//! together: every outstanding transaction sits in exactly one place, the counters are
//! canonical, the held `ENTER` matches the CPU and the bus, the running process matches
//! the process table, the frame bitmap matches its owners, and the boot and shutdown
//! states are consistent. The runtime restores each component from its own bytes only
//! (`docs/m0-design.md` §7), so these checks are the acceptance tooling's, not restore's.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use systemscope_contracts::canonical::{CanonicalEvent, Decoder};
use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::block_v0::BlockMsg;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_rv32::m3ref::{self, BLK, BUS, CPU, DISK, IRQC, KERNEL, RAM, UART};

/// The first bytes of every runtime snapshot.
pub const MAGIC: [u8; 8] = *b"SSSNAP\0\0";
/// The runtime snapshot layout version.
pub const FORMAT_VERSION: u32 = 1;
/// Each component's snapshot schema, in id order (§9.1): the `M3` CPU writes schema 3,
/// every other component schema 1.
pub const SCHEMAS: [u32; 8] = [3, 1, 1, 1, 1, 1, 1, 1];
/// The bus masters, in bus order, and the component behind each.
pub const MASTERS: [ComponentId; 3] = [CPU, BLK, KERNEL];
/// The bus regions, in bus order, and the component behind each.
pub const TARGETS: [ComponentId; 5] = [RAM, UART, IRQC, BLK, KERNEL];
/// The `kgate` region's index.
pub const KGATE: usize = 4;
/// The kernel's disk-boot marker (§6.8).
pub const DISK_MARKER: u8 = 0xD5;
/// The kernel's process-operation tag (§6.8).
pub const PROC_OP: u8 = 2;
/// A page, and a RAM page in the RAM's snapshot.
pub const PAGE_BYTES: u64 = 4096;
/// Where RAM starts: the kernel megapage and every user frame are in it (§11.2).
pub const RAM_BASE: u64 = 0x8000_0000;
/// Machine, supervisor, and user privilege, as the CPU encodes them.
const PRIV_M: u8 = 3;
const PRIV_S: u8 = 1;
const PRIV_U: u8 = 0;
/// The block controller's beat and block sizes and a WRITE's operation code (M2 §9).
const BEAT: u64 = 16;
const BEATS: u8 = 32;
const BLOCK: u64 = 512;
const OP_WRITE: u8 = 2;
/// The kernel's largest output access (§6.8).
const OUTPUT_CHUNK: u32 = 16;

/// A component's entry in the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its id.
    pub id: u32,
    /// Its schema.
    pub schema: u32,
    /// Its bytes in the snapshot.
    pub state: Range<usize>,
}

/// A Sv32 walk in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walk {
    /// `None` for a fetch, else the load or store instruction.
    pub insn: Option<u32>,
    /// The PTE level: 1 or 0.
    pub level: u8,
    /// The table's physical address.
    pub table: u32,
}

/// A walk's purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Purpose {
    /// An instruction fetch.
    Fetch,
    /// A load.
    Load,
    /// A store.
    Store,
}

impl Walk {
    /// What the walk translates for, from the instruction's major opcode.
    pub fn purpose(&self) -> Option<Purpose> {
        match self.insn.map(|i| i & 0x7f) {
            None => Some(Purpose::Fetch),
            Some(0x03) => Some(Purpose::Load),
            Some(0x23) => Some(Purpose::Store),
            Some(_) => None,
        }
    }
}

/// The CPU's execution state (schema 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpuState {
    /// The next fetch is due.
    FetchIssue {
        /// The translated address, if the walk already ran.
        pa: Option<u64>,
    },
    /// A fetch is outstanding.
    FetchWait {
        /// Its `TxnId`.
        txn: u64,
        /// The translated address.
        pa: Option<u64>,
    },
    /// A load or store is due.
    MemIssue {
        /// The instruction.
        insn: u32,
        /// The translated address.
        pa: Option<u64>,
    },
    /// A load or store is outstanding.
    MemWait {
        /// Its `TxnId`.
        txn: u64,
        /// The instruction.
        insn: u32,
        /// The translated address.
        pa: Option<u64>,
    },
    /// A PTE read is due.
    WalkIssue(Walk),
    /// A PTE read is outstanding.
    WalkWait {
        /// Its `TxnId`.
        txn: u64,
        /// The walk.
        walk: Walk,
    },
    /// An outcome waits for `Commit`.
    CommitPending {
        /// The instruction, if one was fetched.
        insn: Option<u32>,
        /// The outcome's tag.
        outcome: u8,
        /// The translated address.
        pa: Option<u64>,
    },
    /// Halted: `Some((cause code, pc, tval))` for a trap, `None` at the instruction
    /// limit.
    Halted(Option<(u8, u32, u32)>),
}

impl CpuState {
    /// The outstanding `TxnId`, if any.
    pub fn txn(&self) -> Option<u64> {
        match self {
            CpuState::FetchWait { txn, .. }
            | CpuState::MemWait { txn, .. }
            | CpuState::WalkWait { txn, .. } => Some(*txn),
            _ => None,
        }
    }
}

/// The CPU (schema 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// `pc`.
    pub pc: u32,
    /// `x1` to `x31`.
    pub regs: [u32; 31],
    /// `instret`.
    pub instret: u64,
    /// The next `TxnId`.
    pub next_txn: u64,
    /// The execution state.
    pub state: CpuState,
    /// `mepc`.
    pub mepc: u32,
    /// `priv`: 0 U, 1 S, 3 M.
    pub privilege: u8,
    /// `mstatus.SPP`.
    pub spp: u8,
    /// `stvec`.
    pub stvec: u32,
    /// `sepc`.
    pub sepc: u32,
    /// `scause`.
    pub scause: u32,
    /// `stval`.
    pub stval: u32,
    /// `satp`.
    pub satp: u32,
}

/// A bus region's active transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Active {
    /// The master's index.
    pub master: u16,
    /// The master's `TxnId`.
    pub original: u64,
    /// The bus's downstream `TxnId`.
    pub downstream: u64,
    /// Whether it is a write.
    pub write: bool,
}

/// A bus region (schema 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Its name.
    pub name: String,
    /// The active transfer.
    pub active: Option<Active>,
    /// The round-robin cursor.
    pub cursor: u16,
    /// Each master's queue, in master order: the queued requests' `TxnId`s.
    pub queues: Vec<Vec<u64>>,
    /// The queued requests' addresses, parallel to `queues`.
    pub addrs: Vec<Vec<u64>>,
}

/// The multi-master bus (schema 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bus {
    /// The master names, in order.
    pub masters: Vec<String>,
    /// The next downstream `TxnId`.
    pub next_downstream: u64,
    /// The regions, in order.
    pub regions: Vec<Region>,
}

/// The block controller's engine position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Nothing to do.
    Idle,
    /// The next step is due.
    Issue,
    /// A media operation is outstanding.
    WaitMedia(u64),
    /// A DMA beat is outstanding.
    WaitBeat(u64),
}

/// The block controller (schema 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blk {
    /// `LBA`, `MEM_ADDR`, `BLOCK_COUNT`, `IRQ_ENABLE`.
    pub registers: [u32; 4],
    /// `BUSY`.
    pub busy: bool,
    /// `DONE`.
    pub done: bool,
    /// The latched operation, `LBA`, `MEM_ADDR`, and `BLOCK_COUNT`, when busy.
    pub latched: Option<(u8, u32, u32, u32)>,
    /// The engine.
    pub engine: Engine,
    /// The block index.
    pub block: u32,
    /// The beat index.
    pub beat: u8,
    /// The buffer's length.
    pub buffer: usize,
    /// The next `dma` `TxnId`.
    pub dma_txn: u64,
    /// The next `blk` `TxnId`.
    pub blk_txn: u64,
}

/// A boot cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    /// The validated table's `(start_lba, byte_len)` entries, once read.
    pub table: Option<Vec<(u32, u32)>>,
    /// The entry index.
    pub index: u32,
}

/// A syscall's user buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer {
    /// The caller's `sepc`.
    pub sepc: u32,
    /// The buffer's address.
    pub buf: u32,
    /// Its length.
    pub n: u32,
}

/// A process operation's stage (§6.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 0: creating `pid` in reserved frames.
    Create {
        /// The PID.
        pid: u32,
        /// The reserved frames.
        frames: Vec<u32>,
    },
    /// 1: reading the trap frame.
    ReadFrame,
    /// 2: writing `pid`'s context to the trap frame.
    Dispatch {
        /// The PID.
        pid: u32,
    },
    /// 3: shutting down.
    Shutdown {
        /// The reason.
        reason: u32,
    },
    /// 4: walking a `write` buffer.
    Walk {
        /// The buffer.
        buffer: Buffer,
        /// The pages found.
        pages: Vec<u32>,
        /// The table PPN.
        table: u32,
        /// The level.
        level: u8,
    },
    /// 5: writing a buffer to the UART.
    Output {
        /// The buffer.
        buffer: Buffer,
        /// Its pages.
        pages: Vec<u32>,
        /// The bytes already at the UART.
        done: u32,
    },
    /// 6: writing a result.
    Return {
        /// The caller's `sepc`.
        sepc: u32,
        /// The result.
        value: u32,
    },
    /// 7: programming the block controller.
    Command(Cursor),
    /// 8: polling `STATUS`.
    Poll(Cursor),
    /// 9: writing `ACK`.
    Ack(Cursor, u8),
    /// 10: reading staging.
    Staged(Cursor, u32),
    /// 11: loading an executable.
    Load {
        /// Where boot is.
        cursor: Cursor,
        /// The header bytes' length.
        headers: usize,
        /// The reserved frames.
        frames: Vec<u32>,
    },
}

impl Stage {
    /// The name the kernel's view uses.
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Create { .. } => "create",
            Stage::ReadFrame => "read_frame",
            Stage::Dispatch { .. } => "dispatch",
            Stage::Shutdown { .. } => "shutdown",
            Stage::Walk { .. } => "walk",
            Stage::Output { .. } => "output",
            Stage::Return { .. } => "return",
            Stage::Command(_) => "blk_command",
            Stage::Poll(_) => "blk_poll",
            Stage::Ack(..) => "blk_ack",
            Stage::Staged(..) => "read_staging",
            Stage::Load { .. } => "load",
        }
    }

    /// A boot stage: reading the table and the executables, or creating a process.
    pub fn is_boot(&self) -> bool {
        matches!(
            self,
            Stage::Create { .. }
                | Stage::Command(_)
                | Stage::Poll(_)
                | Stage::Ack(..)
                | Stage::Staged(..)
                | Stage::Load { .. }
        )
    }

    /// A `write` syscall stage.
    pub fn is_syscall(&self) -> bool {
        matches!(
            self,
            Stage::Walk { .. } | Stage::Output { .. } | Stage::Return { .. }
        )
    }

    /// The frames a creation in flight owns.
    pub fn creation_frames(&self) -> &[u32] {
        match self {
            Stage::Create { frames, .. } | Stage::Load { frames, .. } => frames,
            _ => &[],
        }
    }
}

/// The kernel's operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KernelOp {
    /// The stage.
    pub stage: Stage,
    /// The step within it.
    pub step: u32,
    /// The working data's length.
    pub data: usize,
}

/// A process's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Queued.
    Ready,
    /// On the CPU.
    Running,
    /// Exited with a status.
    Exited(u32),
    /// Killed: `(cause, epc, tval)`.
    Faulted(u32, u32, u32),
}

/// A PCB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pcb {
    /// The PID.
    pub pid: u32,
    /// The state.
    pub state: ProcState,
    /// Whether it holds a saved context.
    pub context: bool,
    /// The root table's PPN.
    pub root: u32,
    /// The page tables' PPNs, the root first.
    pub tables: Vec<u32>,
    /// The regions: `va`, permission bits, and frames.
    pub regions: Vec<(u32, u8, Vec<u32>)>,
}

impl Pcb {
    /// Every frame it owns: the tables, then the regions' frames.
    pub fn frames(&self) -> impl Iterator<Item = u32> + '_ {
        self.tables
            .iter()
            .copied()
            .chain(self.regions.iter().flat_map(|r| r.2.iter().copied()))
    }

    /// Exited or faulted.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, ProcState::Exited(_) | ProcState::Faulted(..))
    }
}

/// The kernel's engine state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelState {
    /// No operation.
    Idle,
    /// The next access is due.
    Issue,
    /// An access is outstanding.
    Wait(u64),
}

/// The modeled kernel (schema 1, disk boot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kernel {
    /// The trap frame's address.
    pub trap_frame: u32,
    /// The frame pool: base and size.
    pub pool: (u64, u64),
    /// The engine state.
    pub state: KernelState,
    /// The operation, unless idle.
    pub op: Option<KernelOp>,
    /// The held `ENTER`'s downstream `TxnId`.
    pub held: Option<u64>,
    /// The next `mem` `TxnId`.
    pub next_txn: u64,
    /// The life: 0 awaiting boot, 1 up, 2 down.
    pub life: u8,
    /// Table entries: 0 until the table is validated.
    pub entries: u32,
    /// The PCBs, in PID order.
    pub pcbs: Vec<Pcb>,
    /// The run queue, head first.
    pub queue: Vec<u32>,
    /// The running PID.
    pub current: Option<u32>,
    /// The frame bitmap.
    pub bitmap: Vec<u8>,
}

impl Kernel {
    /// The pool's first PPN.
    pub fn pool_base(&self) -> u32 {
        (self.pool.0 >> 12) as u32
    }

    /// Frames in the pool.
    pub fn pool_frames(&self) -> usize {
        (self.pool.1 >> 12) as usize
    }

    /// Whether frame `i` of the pool is allocated.
    pub fn allocated(&self, i: usize) -> bool {
        self.bitmap
            .get(i / 8)
            .is_some_and(|b| b >> (i % 8) & 1 == 1)
    }
}

/// A decoded `m3-reference` runtime snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    /// The session seed.
    pub seed: u64,
    /// The last dispatched event.
    pub last_dispatched: Option<EventKey>,
    /// The next sequence number.
    pub next_sequence: u64,
    /// The events in flight, in stored order.
    pub queue: Vec<CanonicalEvent>,
    /// The component entries.
    pub entries: Vec<Entry>,
    /// The CPU.
    pub cpu: Cpu,
    /// The bus.
    pub bus: Bus,
    /// The RAM's pages with a non-zero byte: index and bytes.
    /// RAM's stored pages, by index from the RAM base; absent pages read as zero.
    pub ram: BTreeMap<u32, Vec<u8>>,
    /// The UART's output.
    pub uart: Vec<u8>,
    /// The IRQ controller's `pending`, `enable`, and `out`.
    pub irqc: (u32, u32, bool),
    /// The block controller.
    pub blk: Blk,
    /// The media's stored LBAs.
    pub disk: Vec<u64>,
    /// The kernel.
    pub kernel: Kernel,
    /// Offsets of the fields the corruption tests change, by name.
    pub offsets: BTreeMap<String, usize>,
}

/// Reads fields and remembers where each named one starts.
struct Reader<'a> {
    d: Decoder<'a>,
    base: usize,
    len: usize,
    what: &'static str,
    offsets: &'a mut BTreeMap<String, usize>,
}

type Read<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn new(
        bytes: &'a [u8],
        base: usize,
        what: &'static str,
        offsets: &'a mut BTreeMap<String, usize>,
    ) -> Reader<'a> {
        Reader {
            d: Decoder::new(bytes),
            base,
            len: bytes.len(),
            what,
            offsets,
        }
    }

    fn at(&self) -> usize {
        self.base + self.len - self.d.remaining()
    }

    fn mark(&mut self, name: &str) {
        let at = self.at();
        self.offsets.insert(format!("{}.{name}", self.what), at);
    }

    fn err<E: std::fmt::Debug>(&self, e: E) -> String {
        format!("{} at byte {}: {e:?}", self.what, self.at())
    }

    fn u8(&mut self) -> Read<u8> {
        self.d.u8().map_err(|e| self.err(e))
    }
    fn u16(&mut self) -> Read<u16> {
        self.d.u16().map_err(|e| self.err(e))
    }
    fn u32(&mut self) -> Read<u32> {
        self.d.u32().map_err(|e| self.err(e))
    }
    fn u64(&mut self) -> Read<u64> {
        self.d.u64().map_err(|e| self.err(e))
    }
    fn bool(&mut self) -> Read<bool> {
        self.d.bool().map_err(|e| self.err(e))
    }
    fn array(&mut self) -> Read<[u8; 32]> {
        self.d.array().map_err(|e| self.err(e))
    }
    fn raw(&mut self, n: usize) -> Read<&'a [u8]> {
        self.d.raw(n).map_err(|e| self.err(e))
    }
    fn str(&mut self) -> Read<&'a str> {
        self.d.str().map_err(|e| self.err(e))
    }
    fn bytes(&mut self) -> Read<&'a [u8]> {
        self.d.bytes().map_err(|e| self.err(e))
    }
    /// A count of elements at least `min` bytes each, bounded by what remains.
    fn count(&mut self, min: usize) -> Read<usize> {
        let n = self.d.len().map_err(|e| self.err(e))?;
        if n.saturating_mul(min) > self.d.remaining() {
            return Err(self.err(format!("a count of {n} past the end")));
        }
        Ok(n)
    }
    fn tag(&mut self, what: &str, max: u8) -> Read<u8> {
        let t = self.u8()?;
        if t > max {
            return Err(self.err(format!("{what} tag {t}")));
        }
        Ok(t)
    }
    fn option<T>(&mut self, what: &str, f: impl FnOnce(&mut Self) -> Read<T>) -> Read<Option<T>> {
        match self.tag(what, 1)? {
            0 => Ok(None),
            _ => f(self).map(Some),
        }
    }
    fn latency(&mut self) -> Read<()> {
        match self.tag("latency", 1)? {
            0 => self.raw(16).map(drop),
            _ => self.raw(12).map(drop),
        }
    }
    fn ppns(&mut self) -> Read<Vec<u32>> {
        let n = self.count(4)?;
        (0..n).map(|_| self.u32()).collect()
    }
    fn finish(self) -> Read<()> {
        let at = self.at();
        let what = self.what;
        self.d
            .finish()
            .map_err(|e| format!("{what} at byte {at}: {e:?}"))
    }
}

impl Platform {
    /// Decodes every field of `bytes`, checking encodings only.
    pub fn decode(bytes: &[u8]) -> Result<Platform, String> {
        let mut offsets = BTreeMap::new();
        let mut r = Reader::new(bytes, 0, "runtime", &mut offsets);
        r.mark("magic");
        if r.raw(8)? != MAGIC {
            return Err("runtime: bad magic".to_owned());
        }
        r.mark("format_version");
        let version = r.u32()?;
        if version != FORMAT_VERSION {
            return Err(format!("runtime: format version {version}"));
        }
        let seed = r.u64()?;
        r.raw(16)?; // ticks per second, S5 limit
        r.str()?;
        let domains = r.count(29)?;
        for _ in 0..domains {
            r.raw(28)?;
            r.tag("rounding", 1)?;
        }
        r.array()?; // topology hash
        let last_dispatched = r.option("last dispatched", |r| {
            EventKey::decode(&mut r.d).map_err(|e| r.err(e))
        })?;
        r.u64()?; // dispatched in phase
        let next_sequence = r.u64()?;
        r.array()?; // execution digest
        r.mark("queue");
        let n = r.count(1)?;
        let mut queue = Vec::with_capacity(n);
        for _ in 0..n {
            queue.push(CanonicalEvent::decode(&mut r.d).map_err(|e| r.err(e))?);
        }
        r.mark("rng_count");
        let n = r.count(32)?;
        for _ in 0..n {
            r.raw(32)?;
        }
        r.mark("component_count");
        let n = r.count(12)?;
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            r.mark(&format!("component{i}.id"));
            let id = r.u32()?;
            r.mark(&format!("component{i}.schema"));
            let schema = r.u32()?;
            r.mark(&format!("component{i}.len"));
            let len = r.count(1)?;
            let start = r.at();
            r.raw(len)?;
            entries.push(Entry {
                id,
                schema,
                state: start..start + len,
            });
        }
        r.finish()?;
        if entries.len() != SCHEMAS.len() {
            return Err(format!(
                "runtime: {} component entries, not {}",
                entries.len(),
                SCHEMAS.len()
            ));
        }
        for (i, e) in entries.iter().enumerate() {
            if e.id as usize != i {
                return Err(format!("runtime: entry {i} has id {}", e.id));
            }
            if e.schema != SCHEMAS[i] {
                return Err(format!(
                    "runtime: {} has schema {}, not {}",
                    m3ref::PATHS[i],
                    e.schema,
                    SCHEMAS[i]
                ));
            }
        }
        let part = |id: ComponentId| {
            let range = entries[id.0 as usize].state.clone();
            (&bytes[range.clone()], range.start)
        };
        let (b, at) = part(CPU);
        let cpu = cpu(Reader::new(b, at, "cpu", &mut offsets))?;
        let (b, at) = part(BUS);
        let bus = bus(Reader::new(b, at, "bus", &mut offsets))?;
        let (b, at) = part(RAM);
        let ram = ram(Reader::new(b, at, "ram", &mut offsets))?;
        let (b, at) = part(UART);
        let uart = uart(Reader::new(b, at, "uart", &mut offsets))?;
        let (b, at) = part(IRQC);
        let irqc = irqc(Reader::new(b, at, "irqc", &mut offsets))?;
        let (b, at) = part(BLK);
        let blk = blk(Reader::new(b, at, "blk", &mut offsets))?;
        let (b, at) = part(DISK);
        let disk = disk(Reader::new(b, at, "disk", &mut offsets))?;
        let (b, at) = part(KERNEL);
        let kernel = kernel(Reader::new(b, at, "kernel", &mut offsets))?;
        Ok(Platform {
            seed,
            last_dispatched,
            next_sequence,
            queue,
            entries,
            cpu,
            bus,
            ram,
            uart,
            irqc,
            blk,
            disk,
            kernel,
            offsets,
        })
    }

    /// The offset of a field [`Platform::decode`] marked.
    pub fn offset(&self, name: &str) -> Option<usize> {
        self.offsets.get(name).copied()
    }
}

fn cpu(mut r: Reader<'_>) -> Read<Cpu> {
    r.raw(16)?; // clock, entry, instruction limit
    let pc = r.u32()?;
    let mut regs = [0; 31];
    for reg in &mut regs {
        *reg = r.u32()?;
    }
    let instret = r.u64()?;
    r.mark("next_txn");
    let next_txn = r.u64()?;
    r.mark("state");
    let pa = |r: &mut Reader<'_>| r.option("physical address", Reader::u64);
    let walk = |r: &mut Reader<'_>| -> Read<Walk> {
        let insn = r.option("walk purpose", Reader::u32)?;
        r.mark("walk.level");
        let level = r.u8()?;
        let table = r.u32()?;
        Ok(Walk { insn, level, table })
    };
    let state = match r.tag("cpu state", 7)? {
        0 => CpuState::FetchIssue { pa: pa(&mut r)? },
        1 => CpuState::FetchWait {
            txn: r.u64()?,
            pa: pa(&mut r)?,
        },
        2 => CpuState::MemIssue {
            insn: r.u32()?,
            pa: pa(&mut r)?,
        },
        3 => CpuState::MemWait {
            txn: r.u64()?,
            insn: r.u32()?,
            pa: pa(&mut r)?,
        },
        4 => {
            let insn = r.option("instruction", Reader::u32)?;
            let outcome = r.tag("outcome", 4)?;
            match outcome {
                0 => {
                    r.option("register write", |r| r.raw(5))?;
                    r.u32()?;
                }
                1 => {
                    r.tag("cause", 13)?;
                    r.u32()?;
                }
                2 => {
                    r.u16()?;
                    r.tag("CSR operation", 2)?;
                    r.bool()?;
                    r.u32()?;
                    r.u8()?;
                    r.u32()?;
                }
                _ => {}
            }
            CpuState::CommitPending {
                insn,
                outcome,
                pa: pa(&mut r)?,
            }
        }
        5 => CpuState::Halted(match r.tag("halt", 1)? {
            0 => Some((r.tag("cause", 13)?, r.u32()?, r.u32()?)),
            _ => None,
        }),
        6 => CpuState::WalkIssue(walk(&mut r)?),
        _ => CpuState::WalkWait {
            txn: r.u64()?,
            walk: walk(&mut r)?,
        },
    };
    for _ in 0..3 {
        r.bool()?;
    }
    r.u32()?; // mtvec
    r.u32()?; // mscratch
    let mepc = r.u32()?;
    r.raw(8)?; // mcause, mtval
    r.bool()?; // irq level
    r.mark("priv");
    let privilege = r.u8()?;
    r.bool()?;
    r.bool()?;
    let spp = r.u8()?;
    let mpp = r.u8()?;
    let modes = [PRIV_U, PRIV_S, PRIV_M];
    if !modes.contains(&privilege) || spp > PRIV_S || !modes.contains(&mpp) {
        return Err(r.err(format!("privilege {privilege}, SPP {spp}, MPP {mpp}")));
    }
    r.bool()?;
    r.bool()?;
    r.u32()?; // medeleg
    let stvec = r.u32()?;
    r.u32()?; // sscratch
    let sepc = r.u32()?;
    let scause = r.u32()?;
    let stval = r.u32()?;
    let satp = r.u32()?;
    r.finish()?;
    Ok(Cpu {
        pc,
        regs,
        instret,
        next_txn,
        state,
        mepc,
        privilege,
        spp,
        stvec,
        sepc,
        scause,
        stval,
        satp,
    })
}

fn bus(mut r: Reader<'_>) -> Read<Bus> {
    let n = r.count(20)?;
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        names.push(r.str()?.to_owned());
        r.raw(16)?;
    }
    let m = r.count(4)?;
    let mut masters = Vec::with_capacity(m);
    for _ in 0..m {
        masters.push(r.str()?.to_owned());
    }
    r.u32()?; // clock
    let next_downstream = r.u64()?;
    let mut regions = Vec::with_capacity(n);
    for name in names {
        r.mark(&format!("{name}.active"));
        let active = r.option("active", |r| {
            Ok(Active {
                master: r.u16()?,
                original: r.u64()?,
                downstream: r.u64()?,
                write: r.bool()?,
            })
        })?;
        let cursor = r.u16()?;
        let mut queues = Vec::with_capacity(m);
        let mut requests = Vec::with_capacity(m);
        for i in 0..m {
            r.mark(&format!("{name}.queue{i}"));
            let q = r.count(1)?;
            let mut txns = Vec::with_capacity(q);
            let mut addrs = Vec::with_capacity(q);
            for _ in 0..q {
                let msg = MemMsg::decode(&mut r.d).map_err(|e| r.err(e))?;
                match msg {
                    MemMsg::ReadReq { txn, addr, .. } | MemMsg::WriteReq { txn, addr, .. } => {
                        txns.push(txn.0);
                        addrs.push(addr);
                    }
                    _ => return Err(r.err("a queued response")),
                }
            }
            queues.push(txns);
            requests.push(addrs);
        }
        regions.push(Region {
            name,
            active,
            cursor,
            queues,
            addrs: requests,
        });
    }
    r.finish()?;
    Ok(Bus {
        masters,
        next_downstream,
        regions,
    })
}

fn ram(mut r: Reader<'_>) -> Read<BTreeMap<u32, Vec<u8>>> {
    r.u64()?; // size
    r.array()?; // image hash
    r.latency()?;
    let n = r.count(8)?;
    let mut pages = BTreeMap::new();
    for _ in 0..n {
        let index = r.u32()?;
        let bytes = r.bytes()?;
        if bytes.len() != PAGE_BYTES as usize || pages.insert(index, bytes.to_vec()).is_some() {
            return Err(r.err(format!("RAM page {index}")));
        }
    }
    r.finish()?;
    Ok(pages)
}

fn uart(mut r: Reader<'_>) -> Read<Vec<u8>> {
    r.latency()?;
    r.mark("output");
    let out = r.bytes()?.to_vec();
    r.finish()?;
    Ok(out)
}

fn irqc(mut r: Reader<'_>) -> Read<(u32, u32, bool)> {
    r.u8()?; // sources
    r.latency()?;
    let v = (r.u32()?, r.u32()?, r.bool()?);
    r.finish()?;
    Ok(v)
}

fn blk(mut r: Reader<'_>) -> Read<Blk> {
    r.u32()?; // clock
    r.latency()?;
    r.raw(24)?; // capacity, DMA window
    let registers = [r.u32()?, r.u32()?, r.u32()?, r.u32()?];
    let busy = r.bool()?;
    let done = r.bool()?;
    r.bool()?; // rejected
    r.u8()?; // error
    let latched = if busy {
        Some((r.u8()?, r.u32()?, r.u32()?, r.u32()?))
    } else {
        None
    };
    r.mark("engine");
    let engine = match r.tag("engine", 3)? {
        0 => Engine::Idle,
        1 => Engine::Issue,
        2 => Engine::WaitMedia(r.u64()?),
        _ => Engine::WaitBeat(r.u64()?),
    };
    r.mark("block");
    let block = r.u32()?;
    r.mark("beat");
    let beat = r.u8()?;
    let buffer = r.bytes()?.len();
    r.mark("dma_txn");
    let dma_txn = r.u64()?;
    let blk_txn = r.u64()?;
    r.bool()?; // irq level
    r.finish()?;
    Ok(Blk {
        registers,
        busy,
        done,
        latched,
        engine,
        block,
        beat,
        buffer,
        dma_txn,
        blk_txn,
    })
}

fn disk(mut r: Reader<'_>) -> Read<Vec<u64>> {
    r.u64()?; // capacity
    r.latency()?;
    let bad = r.count(8)?;
    r.raw(8 * bad)?;
    r.array()?; // image hash
    let n = r.count(12)?;
    let mut lbas = Vec::with_capacity(n);
    for _ in 0..n {
        lbas.push(r.u64()?);
        r.bytes()?;
    }
    r.finish()?;
    Ok(lbas)
}

fn cursor(r: &mut Reader<'_>) -> Read<Cursor> {
    let table = r.option("boot table", |r| {
        let n = r.count(8)?;
        (0..n).map(|_| Ok((r.u32()?, r.u32()?))).collect()
    })?;
    Ok(Cursor {
        table,
        index: r.u32()?,
    })
}

fn buffer(r: &mut Reader<'_>) -> Read<Buffer> {
    Ok(Buffer {
        sepc: r.u32()?,
        buf: r.u32()?,
        n: r.u32()?,
    })
}

fn kernel(mut r: Reader<'_>) -> Read<Kernel> {
    r.u32()?; // clock
    r.latency()?;
    r.raw(32)?; // RAM and kgate windows
    let trap_frame = r.u32()?;
    r.raw(16)?; // staging
    let pool = (r.u64()?, r.u64()?);
    r.raw(24)?; // block controller, UART TX
    if r.u8()? != DISK_MARKER {
        return Err(r.err("not a disk-boot kernel"));
    }
    r.raw(16)?; // layout and capacity
    r.mark("state");
    let state = match r.tag("kernel state", 2)? {
        0 => KernelState::Idle,
        1 => KernelState::Issue,
        _ => KernelState::Wait(r.u64()?),
    };
    let op = if state == KernelState::Idle {
        None
    } else {
        r.mark("op");
        if r.u8()? != PROC_OP {
            return Err(r.err("a prototype operation"));
        }
        r.mark("stage");
        let stage = match r.tag("stage", 11)? {
            0 => Stage::Create {
                pid: r.u32()?,
                frames: r.ppns()?,
            },
            1 => Stage::ReadFrame,
            2 => {
                let pid = r.u32()?;
                r.raw(33 * 4)?;
                Stage::Dispatch { pid }
            }
            3 => Stage::Shutdown { reason: r.u32()? },
            4 => Stage::Walk {
                buffer: buffer(&mut r)?,
                pages: r.ppns()?,
                table: r.u32()?,
                level: r.u8()?,
            },
            5 => {
                let buffer = buffer(&mut r)?;
                let pages = r.ppns()?;
                r.mark("output.done");
                Stage::Output {
                    buffer,
                    pages,
                    done: r.u32()?,
                }
            }
            6 => Stage::Return {
                sepc: r.u32()?,
                value: r.u32()?,
            },
            7 => Stage::Command(cursor(&mut r)?),
            8 => Stage::Poll(cursor(&mut r)?),
            9 => Stage::Ack(cursor(&mut r)?, r.u8()?),
            10 => Stage::Staged(cursor(&mut r)?, r.u32()?),
            _ => Stage::Load {
                cursor: cursor(&mut r)?,
                headers: r.bytes()?.len(),
                frames: r.ppns()?,
            },
        };
        r.mark("step");
        let step = r.u32()?;
        let data = r.bytes()?.len();
        Some(KernelOp { stage, step, data })
    };
    r.mark("held");
    let held = r.option("held ENTER", Reader::u64)?;
    r.mark("next_txn");
    let next_txn = r.u64()?;
    r.mark("life");
    let life = r.tag("life", 2)?;
    let entries = r.u32()?;
    let n = r.count(4)?;
    let mut pcbs = Vec::with_capacity(n);
    for i in 0..n {
        r.mark(&format!("pcb{i}.pid"));
        let pid = r.u32()?;
        r.mark(&format!("pcb{i}.state"));
        let state = match r.tag("process state", 3)? {
            0 => ProcState::Ready,
            1 => ProcState::Running,
            2 => ProcState::Exited(r.u32()?),
            _ => ProcState::Faulted(r.u32()?, r.u32()?, r.u32()?),
        };
        let context = r.option("context", |r| r.raw(33 * 4))?.is_some();
        let root = r.u32()?;
        let tables = r.ppns()?;
        let regions_n = r.count(9)?;
        let mut regions = Vec::with_capacity(regions_n);
        for _ in 0..regions_n {
            let va = r.u32()?;
            let perms = r.u8()?;
            if perms & !0b1110 != 0 {
                return Err(r.err(format!("permission bits {perms:#x}")));
            }
            regions.push((va, perms, r.ppns()?));
        }
        pcbs.push(Pcb {
            pid,
            state,
            context,
            root,
            tables,
            regions,
        });
    }
    r.mark("queue");
    let queue = r.ppns()?;
    r.mark("current");
    let current = r.option("running PID", Reader::u32)?;
    r.mark("bitmap");
    let bitmap = r.bytes()?.to_vec();
    r.finish()?;
    Ok(Kernel {
        trap_frame,
        pool,
        state,
        op,
        held,
        next_txn,
        life,
        entries,
        pcbs,
        queue,
        current,
        bitmap,
    })
}

/// Where an outstanding master transaction is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    /// Its request is on its way to the bus.
    Request,
    /// Queued at the bus behind another master.
    Queued,
    /// Its region's active transfer, the downstream half at `At`.
    Active(At),
    /// Its response is on its way back.
    Response,
}

/// Where an active transfer's downstream half is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum At {
    /// Its request is on its way to the target.
    Request,
    /// The kernel holds it: the held `ENTER`.
    Held,
    /// Its response is on its way to the bus.
    Response,
}

/// Every transaction the platform holds, each found in exactly one place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    /// The CPU's outstanding request and where it is.
    pub cpu: Option<(u64, Place)>,
    /// The block controller's outstanding DMA beat and where it is.
    pub dma: Option<(u64, Place)>,
    /// The kernel's outstanding access and where it is.
    pub kernel: Option<(u64, Place)>,
    /// The block controller's outstanding media operation, and whether its result is on
    /// its way back.
    pub media: Option<(u64, bool)>,
    /// The held `ENTER`'s downstream `TxnId`.
    pub held: Option<u64>,
}

impl Accounting {
    /// The outstanding master transactions, as `(master index, txn, place)`.
    pub fn masters(&self) -> Vec<(usize, u64, Place)> {
        [self.cpu, self.dma, self.kernel]
            .into_iter()
            .enumerate()
            .filter_map(|(m, o)| o.map(|(t, p)| (m, t, p)))
            .collect()
    }
}

fn mem_txn(ev: &CanonicalEvent) -> Option<(u64, bool)> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(m),
            ..
        } => Some(match m {
            MemMsg::ReadReq { txn, .. } | MemMsg::WriteReq { txn, .. } => (txn.0, true),
            MemMsg::ReadResp { txn, .. } | MemMsg::WriteResp { txn, .. } => (txn.0, false),
        }),
        _ => None,
    }
}

fn block_txn(ev: &CanonicalEvent) -> Option<(u64, bool)> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::Block(b),
            ..
        } => Some(match b {
            BlockMsg::ReadBlock { txn, .. } | BlockMsg::WriteBlock { txn, .. } => (txn.0, true),
            BlockMsg::ReadResult { txn, .. } | BlockMsg::WriteResult { txn, .. } => (txn.0, false),
        }),
        _ => None,
    }
}

/// The mem and block messages in flight, keyed by direction.
#[derive(Default)]
struct InFlight {
    /// `(master, txn)` requests on their way to the bus.
    up_req: BTreeMap<(usize, u64), usize>,
    /// `(master, txn)` responses on their way from the bus.
    up_resp: BTreeMap<(usize, u64), usize>,
    /// `(region, txn)` requests on their way to a target.
    down_req: BTreeMap<(usize, u64), usize>,
    /// `(region, txn)` responses on their way to the bus.
    down_resp: BTreeMap<(usize, u64), usize>,
    /// Media requests, by txn.
    media_req: BTreeMap<u64, usize>,
    /// Media results, by txn.
    media_resp: BTreeMap<u64, usize>,
}

fn index_of(list: &[ComponentId], id: ComponentId) -> Option<usize> {
    list.iter().position(|&c| c == id)
}

impl InFlight {
    fn of(queue: &[CanonicalEvent]) -> Result<InFlight, String> {
        let mut f = InFlight::default();
        for ev in queue {
            let route = (ev.source, ev.target);
            if let Some((txn, request)) = mem_txn(ev) {
                let (map, key) = match (route, request) {
                    ((m, BUS), true) => (&mut f.up_req, index_of(&MASTERS, m)),
                    ((BUS, m), false) => (&mut f.up_resp, index_of(&MASTERS, m)),
                    ((BUS, t), true) => (&mut f.down_req, index_of(&TARGETS, t)),
                    ((t, BUS), false) => (&mut f.down_resp, index_of(&TARGETS, t)),
                    _ => (&mut f.up_req, None),
                };
                let key = key.ok_or_else(|| format!("a mem.v1 message on no bus link: {ev:?}"))?;
                *map.entry((key, txn)).or_default() += 1;
            } else if let Some((txn, request)) = block_txn(ev) {
                let map = match (route, request) {
                    ((BLK, DISK), true) => &mut f.media_req,
                    ((DISK, BLK), false) => &mut f.media_resp,
                    _ => return Err(format!("a block.v0 message on no media link: {ev:?}")),
                };
                *map.entry(txn).or_default() += 1;
            }
        }
        Ok(f)
    }
}

impl Platform {
    /// The platform-level checks, on the decoded values together; returns where every
    /// outstanding transaction is.
    pub fn check(&self) -> Result<Accounting, String> {
        let acc = self.check_transactions()?;
        self.check_gate(&acc)?;
        self.check_cpu()?;
        self.check_dma(&acc)?;
        self.check_output()?;
        self.check_processes()?;
        self.check_frames()?;
        self.check_life(&acc)?;
        Ok(acc)
    }

    fn check_transactions(&self) -> Result<Accounting, String> {
        let bus = &self.bus;
        if bus.masters.len() != MASTERS.len() || bus.regions.len() != TARGETS.len() {
            return Err("the bus is not m3-reference's".to_owned());
        }
        let f = InFlight::of(&self.queue)?;
        let blk = &self.blk;
        let k = &self.kernel;
        let outstanding: [Option<u64>; 3] = [
            self.cpu.state.txn(),
            match blk.engine {
                Engine::WaitBeat(t) => Some(t),
                _ => None,
            },
            match k.state {
                KernelState::Wait(t) => Some(t),
                _ => None,
            },
        ];
        let counters = [self.cpu.next_txn, blk.dma_txn, k.next_txn];
        let names = ["cpu", "dma", "kernel"];
        // Every master transaction the platform holds anywhere.
        let mut seen: BTreeMap<(usize, u64), Vec<Place>> = BTreeMap::new();
        for (&key, &n) in &f.up_req {
            seen.entry(key)
                .or_default()
                .extend((0..n).map(|_| Place::Request));
        }
        for (&key, &n) in &f.up_resp {
            seen.entry(key)
                .or_default()
                .extend((0..n).map(|_| Place::Response));
        }
        let mut downstream = BTreeMap::new();
        for (r, region) in bus.regions.iter().enumerate() {
            if region.queues.len() != MASTERS.len() {
                return Err(format!("bus region {} has the wrong queues", region.name));
            }
            for (m, q) in region.queues.iter().enumerate() {
                for &t in q {
                    seen.entry((m, t)).or_default().push(Place::Queued);
                }
            }
            if let Some(a) = region.active {
                let m = usize::from(a.master);
                if m >= MASTERS.len() {
                    return Err(format!("bus region {} active for master {m}", region.name));
                }
                if a.downstream >= bus.next_downstream {
                    return Err(format!(
                        "bus region {}: downstream txn {} not below the counter {}",
                        region.name, a.downstream, bus.next_downstream
                    ));
                }
                if downstream.insert(a.downstream, r).is_some() {
                    return Err(format!("downstream txn {} active twice", a.downstream));
                }
                let places = f.down_req.get(&(r, a.downstream)).copied().unwrap_or(0)
                    + usize::from(r == KGATE && k.held == Some(a.downstream))
                    + f.down_resp.get(&(r, a.downstream)).copied().unwrap_or(0);
                if places != 1 {
                    return Err(format!(
                        "bus region {}'s downstream txn {} is in {places} places",
                        region.name, a.downstream
                    ));
                }
                let at = if f.down_req.contains_key(&(r, a.downstream)) {
                    At::Request
                } else if f.down_resp.contains_key(&(r, a.downstream)) {
                    At::Response
                } else {
                    At::Held
                };
                seen.entry((m, a.original))
                    .or_default()
                    .push(Place::Active(at));
            }
        }
        for key in f.down_req.keys().chain(f.down_resp.keys()) {
            if downstream.get(&key.1) != Some(&key.0) {
                return Err(format!(
                    "downstream txn {} to {} belongs to no active transfer",
                    key.1, bus.regions[key.0].name
                ));
            }
        }
        let mut places: [Option<(u64, Place)>; 3] = [None; 3];
        for (m, out) in outstanding.iter().enumerate() {
            if let Some(t) = out
                && counters[m].checked_sub(1) != Some(*t)
            {
                return Err(format!(
                    "{}'s outstanding txn {t} is not the latest of its counter {}",
                    names[m], counters[m]
                ));
            }
        }
        for (&(m, t), found) in &seen {
            if outstanding[m] != Some(t) {
                return Err(format!(
                    "{} txn {t} at {found:?}, but {} has no such request outstanding",
                    names[m], names[m]
                ));
            }
            if found.len() != 1 {
                return Err(format!("{} txn {t} is in {found:?}", names[m]));
            }
            places[m] = Some((t, found[0]));
        }
        for (m, out) in outstanding.iter().enumerate() {
            if out.is_some() && places[m].is_none() {
                return Err(format!(
                    "{}'s outstanding txn {out:?} is nowhere: it would never complete",
                    names[m]
                ));
            }
        }
        let media = match blk.engine {
            Engine::WaitMedia(t) => {
                if blk.blk_txn.checked_sub(1) != Some(t) {
                    return Err(format!("media txn {t} is not the latest issued"));
                }
                let req = f.media_req.get(&t).copied().unwrap_or(0);
                let resp = f.media_resp.get(&t).copied().unwrap_or(0);
                if req + resp != 1 || f.media_req.len() + f.media_resp.len() != 1 {
                    return Err(format!("media txn {t}: {req} requests, {resp} results"));
                }
                Some((t, resp == 1))
            }
            _ => {
                if !f.media_req.is_empty() || !f.media_resp.is_empty() {
                    return Err("a media message with no operation outstanding".to_owned());
                }
                None
            }
        };
        Ok(Accounting {
            cpu: places[0],
            dma: places[1],
            kernel: places[2],
            media,
            held: k.held,
        })
    }

    fn check_gate(&self, acc: &Accounting) -> Result<(), String> {
        let k = &self.kernel;
        let gate = self.bus.regions[KGATE].active;
        if k.held.is_some() != k.op.is_some() {
            return Err("a held ENTER without an operation, or the reverse".to_owned());
        }
        if let Some(h) = k.held {
            let Some(a) = gate.filter(|a| a.downstream == h) else {
                return Err(format!("the held ENTER {h} is not kgate's active transfer"));
            };
            if usize::from(a.master) != 0 || !a.write {
                return Err("the held ENTER is not the CPU's store".to_owned());
            }
            match self.cpu.state {
                CpuState::MemWait { txn, .. } if txn == a.original => {}
                _ => {
                    return Err(format!(
                        "the held ENTER's CPU store {} is not the CPU's outstanding access",
                        a.original
                    ));
                }
            }
            if acc.cpu != Some((a.original, Place::Active(At::Held))) {
                return Err("the held ENTER is not where the CPU's store is".to_owned());
            }
            // kgate is only in the kernel megapage (U = 0): only S stores to it.
            if self.cpu.privilege != PRIV_S {
                return Err(format!(
                    "a held ENTER from privilege {}",
                    self.cpu.privilege
                ));
            }
        }
        Ok(())
    }

    /// The word at physical address `pa` in RAM, if `pa` is in it.
    pub fn ram_word(&self, pa: u64) -> Option<u32> {
        let offset = pa.checked_sub(RAM_BASE)?;
        let page = u32::try_from(offset / PAGE_BYTES).ok()?;
        let at = (offset % PAGE_BYTES) as usize;
        let word = match self.ram.get(&page) {
            Some(bytes) => bytes.get(at..at + 4)?.try_into().ok()?,
            None => [0; 4],
        };
        Some(u32::from_le_bytes(word))
    }

    fn register(&self, i: u32) -> u32 {
        match i {
            0 => 0,
            _ => self.cpu.regs[i as usize - 1],
        }
    }

    /// The CPU against the address space it runs in (§5, §11.2): U code is below RAM and
    /// S and M code in it, since U cannot fetch from the kernel megapage and S cannot
    /// fetch from a U page; a walk starts at `satp`'s root and descends through the
    /// level-1 PTE of its address.
    fn check_cpu(&self) -> Result<(), String> {
        let cpu = &self.cpu;
        if (cpu.privilege == PRIV_U) != (u64::from(cpu.pc) < RAM_BASE) {
            return Err(format!(
                "privilege {} at pc {:#010x}",
                cpu.privilege, cpu.pc
            ));
        }
        let walk = match &cpu.state {
            CpuState::WalkIssue(walk) | CpuState::WalkWait { walk, .. } => walk,
            _ => return Ok(()),
        };
        if cpu.satp >> 31 == 0 || cpu.privilege == PRIV_M {
            return Err("a walk with translation off".to_owned());
        }
        let root = cpu.satp & 0x003F_FFFF;
        let va = match (walk.insn, walk.purpose()) {
            (None, _) => cpu.pc,
            (Some(insn), Some(Purpose::Load)) => self
                .register((insn >> 15) & 31)
                .wrapping_add(((insn as i32) >> 20) as u32),
            (Some(insn), Some(Purpose::Store)) => self
                .register((insn >> 15) & 31)
                .wrapping_add(((((insn as i32) >> 25) << 5) as u32) | ((insn >> 7) & 31)),
            _ => return Err("a walk for no load or store".to_owned()),
        };
        let expected = match walk.level {
            1 => root,
            0 => {
                let pte_at = u64::from(root) * PAGE_BYTES + u64::from(va >> 22) * 4;
                let pte = self
                    .ram_word(pte_at)
                    .ok_or_else(|| format!("level-1 PTE at {pte_at:#x} outside RAM"))?;
                if pte & 1 == 0 || pte & 0xE != 0 {
                    return Err(format!("a level-0 walk under the level-1 PTE {pte:#010x}"));
                }
                (pte >> 10) & 0x003F_FFFF
            }
            level => return Err(format!("a walk at level {level}")),
        };
        if walk.table != expected {
            return Err(format!(
                "a level-{} walk of {va:#010x} in table {:#x}, not {expected:#x}",
                walk.level, walk.table
            ));
        }
        Ok(())
    }

    /// The block controller's position against its command and its outstanding beat
    /// (M2 §9.6): a beat is `MEM_ADDR + 512 block + 16 beat`, and a WRITE has buffered
    /// 16 bytes per beat read.
    fn check_dma(&self, acc: &Accounting) -> Result<(), String> {
        let blk = &self.blk;
        let Some((op, _, addr, count)) = blk.latched else {
            if blk.engine != Engine::Idle || blk.block != 0 || blk.beat != 0 {
                return Err("the block controller moves with no command".to_owned());
            }
            return Ok(());
        };
        if blk.block >= count || blk.beat > BEATS {
            return Err(format!(
                "block {} beat {} of a {count}-block command",
                blk.block, blk.beat
            ));
        }
        let beat_addr = u64::from(addr) + u64::from(blk.block) * BLOCK + u64::from(blk.beat) * BEAT;
        if let Engine::WaitBeat(t) = blk.engine {
            if blk.beat >= BEATS {
                return Err(format!("beat {t} outstanding past the block's last",));
            }
            let found =
                match acc.dma {
                    Some((_, Place::Request)) => self.queue.iter().find_map(|ev| {
                        match (ev.source, ev.target, &ev.delivery) {
                            (
                                BLK,
                                BUS,
                                Delivered::Message {
                                    msg:
                                        Message::MemV1(
                                            MemMsg::ReadReq { txn, addr, .. }
                                            | MemMsg::WriteReq { txn, addr, .. },
                                        ),
                                    ..
                                },
                            ) if txn.0 == t => Some(*addr),
                            _ => None,
                        }
                    }),
                    // The bus queues a request translated into its region: the DMA's is RAM.
                    Some((_, Place::Queued)) => self.bus.regions.iter().find_map(|r| {
                        let dma = 1;
                        r.queues[dma]
                            .iter()
                            .position(|&q| q == t)
                            .map(|i| r.addrs[dma][i] + if r.name == "ram" { RAM_BASE } else { 0 })
                    }),
                    _ => None,
                };
            if found.is_some_and(|a| a != beat_addr) {
                return Err(format!(
                    "beat {t} to {:#x}, not block {} beat {}'s {beat_addr:#x}",
                    found.unwrap_or_default(),
                    blk.block,
                    blk.beat
                ));
            }
        }
        let reading = matches!(blk.engine, Engine::Issue | Engine::WaitBeat(_));
        if op == OP_WRITE
            && reading
            && blk.beat < BEATS
            && blk.buffer != usize::from(blk.beat) * BEAT as usize
        {
            return Err(format!(
                "a WRITE at beat {} with {} bytes buffered",
                blk.beat, blk.buffer
            ));
        }
        Ok(())
    }

    /// A `write`'s progress is where an output chunk starts: at most 16 bytes that cross
    /// no page (§6.8).
    fn check_output(&self) -> Result<(), String> {
        let Some(Stage::Output { buffer, done, .. }) = self.kernel.op.as_ref().map(|o| &o.stage)
        else {
            return Ok(());
        };
        let mut at = 0;
        while at < buffer.n {
            if at == *done {
                return Ok(());
            }
            let va = buffer.buf.wrapping_add(at);
            let page_left = PAGE_BYTES as u32 - va % PAGE_BYTES as u32;
            at += (buffer.n - at).min(OUTPUT_CHUNK).min(page_left);
        }
        Err(format!(
            "a write of {} bytes at {done} bytes in, not a chunk start",
            buffer.n
        ))
    }

    fn check_processes(&self) -> Result<(), String> {
        let k = &self.kernel;
        let running: Vec<u32> = k
            .pcbs
            .iter()
            .filter(|p| p.state == ProcState::Running)
            .map(|p| p.pid)
            .collect();
        if running.len() > 1 || k.current != running.first().copied() {
            return Err(format!(
                "the current PID {:?} is not the Running process {running:?}",
                k.current
            ));
        }
        let pids: Vec<u32> = k.pcbs.iter().map(|p| p.pid).collect();
        if pids.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!("the PCBs are not in PID order: {pids:?}"));
        }
        let ready: BTreeSet<u32> = k
            .pcbs
            .iter()
            .filter(|p| p.state == ProcState::Ready)
            .map(|p| p.pid)
            .collect();
        let queued: BTreeSet<u32> = k.queue.iter().copied().collect();
        if queued.len() != k.queue.len() || queued != ready {
            return Err(format!(
                "the run queue {:?} is not the Ready processes {ready:?}",
                k.queue
            ));
        }
        // The CPU owns the running process's registers; a PCB holds a context only while
        // the process is Ready (§6.4).
        for p in &k.pcbs {
            if p.context != (p.state == ProcState::Ready) {
                return Err(format!(
                    "process {} is {:?} with a saved context {}",
                    p.pid, p.state, p.context
                ));
            }
        }
        Ok(())
    }

    fn check_frames(&self) -> Result<(), String> {
        let k = &self.kernel;
        let frames = k.pool_frames();
        if k.bitmap.len() != frames.div_ceil(8) {
            return Err("the frame bitmap is the wrong size".to_owned());
        }
        let mut owned = vec![false; frames];
        let creation = k.op.as_ref().map_or(&[][..], |o| o.stage.creation_frames());
        let live = k.pcbs.iter().filter(|p| !p.is_terminal());
        for ppn in live.flat_map(Pcb::frames).chain(creation.iter().copied()) {
            let i = ppn
                .checked_sub(k.pool_base())
                .map(|i| i as usize)
                .filter(|&i| i < frames)
                .ok_or_else(|| format!("frame {ppn:#x} is outside the pool"))?;
            if std::mem::replace(&mut owned[i], true) {
                return Err(format!("frame {ppn:#x} is owned twice"));
            }
        }
        for (i, &o) in owned.iter().enumerate() {
            if o != k.allocated(i) {
                return Err(format!(
                    "frame {:#x}: allocated {}, owned {o}",
                    k.pool_base() + i as u32,
                    k.allocated(i)
                ));
            }
        }
        if (frames..k.bitmap.len() * 8).any(|i| k.allocated(i)) {
            return Err("a frame bitmap bit past the pool".to_owned());
        }
        Ok(())
    }

    fn check_life(&self, acc: &Accounting) -> Result<(), String> {
        let k = &self.kernel;
        let stage = k.op.as_ref().map(|o| &o.stage);
        match k.life {
            0 => {
                if !k.pcbs.is_empty() || k.entries != 0 || stage.is_some() {
                    return Err("processes, a table, or an operation before boot".to_owned());
                }
            }
            2 => {
                // Only the shutdown's own trap-frame write may still be in progress; once
                // it releases ENTER, nothing is outstanding.
                let finishing = matches!(stage, Some(Stage::Shutdown { .. }));
                if !finishing
                    && (k.state != KernelState::Idle || acc.kernel.is_some() || k.held.is_some())
                {
                    return Err("an access or a held ENTER after shutdown".to_owned());
                }
                if stage.is_some() && !finishing {
                    return Err("an operation other than the shutdown after it".to_owned());
                }
                if k.pcbs.iter().any(|p| !p.is_terminal()) {
                    return Err("a live process after shutdown".to_owned());
                }
            }
            _ => {}
        }
        if let Some(s) = stage
            && s.is_boot()
            && (k.current.is_some() || k.pcbs.iter().any(|p| p.state != ProcState::Ready))
        {
            return Err(format!(
                "boot stage {} with a process that has run",
                s.name()
            ));
        }
        if let Some(s) = stage
            && s.is_syscall()
            && k.current.is_none()
        {
            return Err(format!(
                "syscall stage {} with no process running",
                s.name()
            ));
        }
        Ok(())
    }
}

/// Decodes and checks `bytes`.
pub fn read(bytes: &[u8]) -> Result<(Platform, Accounting), String> {
    let p = Platform::decode(bytes)?;
    let acc = p.check()?;
    Ok((p, acc))
}
