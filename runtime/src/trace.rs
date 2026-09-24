//! Recorded traces and their digest (`docs/m0-design.md` §8.1).
//!
//! A [`Trace`] is the header plus every record in emission order. Its canonical stream
//! encoding is the only source of truth: [`Trace::digest`] hashes that encoding, and the
//! exporters in [`crate::export`] are views of the same records.

use std::fmt;

use systemscope_contracts::canonical::CanonicalEvent;
use systemscope_contracts::component::{ComponentId, Delivered, PortId};
use systemscope_contracts::event::EventKey;
use systemscope_contracts::protocol::block_v0::{
    self, BlockMsg, BlockReadOutcome, BlockWriteOutcome, MediaError,
};
use systemscope_contracts::protocol::irq_v0::{self, IrqMsg};
use systemscope_contracts::protocol::mem::{MemMsg, TxnId};
use systemscope_contracts::protocol::mem_v1::{self, MemFault, ReadOutcome, WriteOutcome};
use systemscope_contracts::protocol::{Message, ProtocolId};
use systemscope_contracts::trace::{
    DISPATCH_KIND, TraceAt, TraceHeader, TraceOrigin, TraceRecord, Value, encode_stream,
};

use crate::runtime::chain;

/// A complete recorded trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    /// Session facts needed to interpret the records.
    pub header: TraceHeader,
    /// Records in emission order.
    pub records: Vec<TraceRecord>,
}

impl Trace {
    /// The canonical stream encoding.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        encode_stream(&self.header, &self.records)
    }

    /// `TraceDigest`: BLAKE3 of the canonical stream encoding.
    pub fn digest(&self) -> [u8; 32] {
        *blake3::hash(&self.canonical_bytes()).as_bytes()
    }
}

/// The `runtime.dispatch` record for an event, emitted before its handler's records.
pub(crate) fn dispatch_record(
    at: TraceAt,
    source: ComponentId,
    target: ComponentId,
    delivery: &Delivered,
) -> TraceRecord {
    let mut fields = vec![("source", Value::U64(u64::from(source.0)))];
    match delivery {
        Delivered::Message { port, msg } => {
            let protocol = msg.protocol();
            fields.push(("port", Value::U64(u64::from(port.0))));
            fields.push(("protocol", Value::Str(protocol.name.to_owned())));
            fields.push(("version", Value::U64(u64::from(protocol.version))));
            message_fields(msg, &mut fields);
        }
        Delivered::Wake { token } => fields.push(("token", Value::U64(*token))),
    }
    TraceRecord {
        at,
        origin: TraceOrigin::Runtime,
        component: target,
        kind: DISPATCH_KIND,
        fields,
    }
}

/// Appends `msg` (the variant name) and the message's fields in declaration order.
fn message_fields(msg: &Message, fields: &mut Vec<(&'static str, Value)>) {
    let mem = match msg {
        Message::Mem(mem) => mem,
        Message::MemV1(mem) => return mem_v1_fields(mem, fields),
        Message::Irq(irq) => return irq_v0_fields(irq, fields),
        Message::Block(block) => return block_v0_fields(block, fields),
    };
    let txn = |t: &systemscope_contracts::protocol::mem::TxnId| Value::U64(t.0);
    match mem {
        MemMsg::ReadReq { txn: t, addr, len } => fields.extend([
            ("msg", Value::Str("ReadReq".into())),
            ("txn", txn(t)),
            ("addr", Value::U64(*addr)),
            ("len", Value::U64(u64::from(*len))),
        ]),
        MemMsg::ReadResp { txn: t, data } => fields.extend([
            ("msg", Value::Str("ReadResp".into())),
            ("txn", txn(t)),
            ("data", Value::Bytes(data.clone())),
        ]),
        MemMsg::WriteReq { txn: t, addr, data } => fields.extend([
            ("msg", Value::Str("WriteReq".into())),
            ("txn", txn(t)),
            ("addr", Value::U64(*addr)),
            ("data", Value::Bytes(data.clone())),
        ]),
        MemMsg::WriteResp { txn: t } => {
            fields.extend([("msg", Value::Str("WriteResp".into())), ("txn", txn(t))])
        }
    }
}

/// The `mem.v1` fields, with an outcome flattened into `outcome` (its variant name)
/// followed by that variant's field (`docs/m1-design.md` §4.3).
fn mem_v1_fields(msg: &mem_v1::MemMsg, fields: &mut Vec<(&'static str, Value)>) {
    let name = |s: &str| Value::Str(s.into());
    let fault = |f: &MemFault| match f {
        MemFault::AccessFault => ("fault", name("AccessFault")),
    };
    match msg {
        mem_v1::MemMsg::ReadReq { txn, addr, len } => fields.extend([
            ("msg", name("ReadReq")),
            ("txn", Value::U64(txn.0)),
            ("addr", Value::U64(*addr)),
            ("len", Value::U64(u64::from(*len))),
        ]),
        mem_v1::MemMsg::ReadResp { txn, outcome } => {
            fields.extend([("msg", name("ReadResp")), ("txn", Value::U64(txn.0))]);
            match outcome {
                ReadOutcome::Data { data } => fields.extend([
                    ("outcome", name("Data")),
                    ("data", Value::Bytes(data.clone())),
                ]),
                ReadOutcome::Fault { fault: f } => {
                    fields.extend([("outcome", name("Fault")), fault(f)])
                }
            }
        }
        mem_v1::MemMsg::WriteReq { txn, addr, data } => fields.extend([
            ("msg", name("WriteReq")),
            ("txn", Value::U64(txn.0)),
            ("addr", Value::U64(*addr)),
            ("data", Value::Bytes(data.clone())),
        ]),
        mem_v1::MemMsg::WriteResp { txn, outcome } => {
            fields.extend([("msg", name("WriteResp")), ("txn", Value::U64(txn.0))]);
            match outcome {
                WriteOutcome::Done => fields.push(("outcome", name("Done"))),
                WriteOutcome::Fault { fault: f } => {
                    fields.extend([("outcome", name("Fault")), fault(f)])
                }
            }
        }
    }
}

/// The `irq.v0` fields (`docs/m2-design.md` §7.1).
fn irq_v0_fields(msg: &IrqMsg, fields: &mut Vec<(&'static str, Value)>) {
    match msg {
        IrqMsg::Level { asserted } => fields.extend([
            ("msg", Value::Str("Level".into())),
            ("asserted", Value::Bool(*asserted)),
        ]),
    }
}

/// The `block.v0` fields, with an outcome flattened into `outcome` (its variant name)
/// followed by that variant's field, as for `mem.v1`.
fn block_v0_fields(msg: &BlockMsg, fields: &mut Vec<(&'static str, Value)>) {
    let name = |s: &str| Value::Str(s.into());
    let error = |e: &MediaError| {
        let e = match e {
            MediaError::OutOfRange => "OutOfRange",
            MediaError::BadBlock => "BadBlock",
        };
        ("error", name(e))
    };
    match msg {
        BlockMsg::ReadBlock { txn, lba } => fields.extend([
            ("msg", name("ReadBlock")),
            ("txn", Value::U64(txn.0)),
            ("lba", Value::U64(*lba)),
        ]),
        BlockMsg::WriteBlock { txn, lba, data } => fields.extend([
            ("msg", name("WriteBlock")),
            ("txn", Value::U64(txn.0)),
            ("lba", Value::U64(*lba)),
            ("data", Value::Bytes(data.clone())),
        ]),
        BlockMsg::ReadResult { txn, outcome } => {
            fields.extend([("msg", name("ReadResult")), ("txn", Value::U64(txn.0))]);
            match outcome {
                BlockReadOutcome::Data { data } => fields.extend([
                    ("outcome", name("Data")),
                    ("data", Value::Bytes(data.clone())),
                ]),
                BlockReadOutcome::Error { error: e } => {
                    fields.extend([("outcome", name("Error")), error(e)])
                }
            }
        }
        BlockMsg::WriteResult { txn, outcome } => {
            fields.extend([("msg", name("WriteResult")), ("txn", Value::U64(txn.0))]);
            match outcome {
                BlockWriteOutcome::Done => fields.push(("outcome", name("Done"))),
                BlockWriteOutcome::Error { error: e } => {
                    fields.extend([("outcome", name("Error")), error(e)])
                }
            }
        }
    }
}

/// Why a trace prefix was not accepted by `resume_trace` (§8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeError {
    /// The runtime was not restored, has already stepped, or has already resumed a trace.
    NotFreshlyRestored,
    /// The prefix's header differs from this session's.
    HeaderMismatch,
    /// The record at this index cannot appear where it does in a trace.
    MalformedRecord(usize),
    /// The prefix's dispatch records do not reproduce the snapshot's execution history.
    HistoryMismatch,
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResumeError::NotFreshlyRestored => {
                f.write_str("a trace resumes only right after restore, before any step")
            }
            ResumeError::HeaderMismatch => f.write_str("trace header differs from the session"),
            ResumeError::MalformedRecord(i) => write!(f, "record {i} is out of place"),
            ResumeError::HistoryMismatch => {
                f.write_str("trace prefix does not match the snapshot's event history")
            }
        }
    }
}

impl std::error::Error for ResumeError {}

/// The event a `runtime.dispatch` record describes, if it is exactly what
/// [`dispatch_record`] writes for that event.
pub(crate) fn dispatched_event(r: &TraceRecord) -> Option<CanonicalEvent> {
    let TraceAt::Event(key) = r.at else {
        return None;
    };
    if r.origin != TraceOrigin::Runtime || r.kind != DISPATCH_KIND {
        return None;
    }
    let get = |i: usize, name: &str| match r.fields.get(i) {
        Some((n, v)) if *n == name => Some(v),
        _ => None,
    };
    let int = |i, name| match get(i, name) {
        Some(Value::U64(v)) => Some(*v),
        _ => None,
    };
    let bytes = |i, name| match get(i, name) {
        Some(Value::Bytes(v)) => Some(v.clone()),
        _ => None,
    };
    let source = ComponentId(u32::try_from(int(0, "source")?).ok()?);
    let delivery = if r.fields.len() == 2 && get(1, "token").is_some() {
        Delivered::Wake {
            token: int(1, "token")?,
        }
    } else {
        let port = PortId(u16::try_from(int(1, "port")?).ok()?);
        let Some(Value::Str(msg)) = get(4, "msg") else {
            return None;
        };
        let str_at = |i, name| match get(i, name) {
            Some(Value::Str(v)) => Some(v.as_str()),
            _ => None,
        };
        let (name, version) = (str_at(2, "protocol")?, int(3, "version")?);
        let is = |p: ProtocolId| name == p.name && version == u64::from(p.version);
        let msg = if is(irq_v0::PROTOCOL) {
            Message::Irq(match (msg.as_str(), get(5, "asserted")?) {
                ("Level", Value::Bool(asserted)) => IrqMsg::Level {
                    asserted: *asserted,
                },
                _ => return None,
            })
        } else if is(block_v0::PROTOCOL) {
            let txn = TxnId(int(5, "txn")?);
            let error = || match str_at(7, "error")? {
                "OutOfRange" => Some(MediaError::OutOfRange),
                "BadBlock" => Some(MediaError::BadBlock),
                _ => None,
            };
            Message::Block(match msg.as_str() {
                "ReadBlock" => BlockMsg::ReadBlock {
                    txn,
                    lba: int(6, "lba")?,
                },
                "WriteBlock" => BlockMsg::WriteBlock {
                    txn,
                    lba: int(6, "lba")?,
                    data: bytes(7, "data")?,
                },
                "ReadResult" => BlockMsg::ReadResult {
                    txn,
                    outcome: match str_at(6, "outcome")? {
                        "Data" => BlockReadOutcome::Data {
                            data: bytes(7, "data")?,
                        },
                        "Error" => BlockReadOutcome::Error { error: error()? },
                        _ => return None,
                    },
                },
                "WriteResult" => BlockMsg::WriteResult {
                    txn,
                    outcome: match str_at(6, "outcome")? {
                        "Done" => BlockWriteOutcome::Done,
                        "Error" => BlockWriteOutcome::Error { error: error()? },
                        _ => return None,
                    },
                },
                _ => return None,
            })
        } else {
            let txn = TxnId(int(5, "txn")?);
            match version {
                0 => Message::Mem(match msg.as_str() {
                    "ReadReq" => MemMsg::ReadReq {
                        txn,
                        addr: int(6, "addr")?,
                        len: u32::try_from(int(7, "len")?).ok()?,
                    },
                    "ReadResp" => MemMsg::ReadResp {
                        txn,
                        data: bytes(6, "data")?,
                    },
                    "WriteReq" => MemMsg::WriteReq {
                        txn,
                        addr: int(6, "addr")?,
                        data: bytes(7, "data")?,
                    },
                    "WriteResp" => MemMsg::WriteResp { txn },
                    _ => return None,
                }),
                1 => {
                    let fault = || match str_at(7, "fault")? {
                        "AccessFault" => Some(MemFault::AccessFault),
                        _ => None,
                    };
                    Message::MemV1(match msg.as_str() {
                        "ReadReq" => mem_v1::MemMsg::ReadReq {
                            txn,
                            addr: int(6, "addr")?,
                            len: u32::try_from(int(7, "len")?).ok()?,
                        },
                        "ReadResp" => mem_v1::MemMsg::ReadResp {
                            txn,
                            outcome: match str_at(6, "outcome")? {
                                "Data" => ReadOutcome::Data {
                                    data: bytes(7, "data")?,
                                },
                                "Fault" => ReadOutcome::Fault { fault: fault()? },
                                _ => return None,
                            },
                        },
                        "WriteReq" => mem_v1::MemMsg::WriteReq {
                            txn,
                            addr: int(6, "addr")?,
                            data: bytes(7, "data")?,
                        },
                        "WriteResp" => mem_v1::MemMsg::WriteResp {
                            txn,
                            outcome: match str_at(6, "outcome")? {
                                "Done" => WriteOutcome::Done,
                                "Fault" => WriteOutcome::Fault { fault: fault()? },
                                _ => return None,
                            },
                        },
                        _ => return None,
                    })
                }
                _ => return None,
            }
        };
        Delivered::Message { port, msg }
    };
    // Rebuilding the record checks every remaining field: protocol, version, and count.
    let rebuilt = dispatch_record(r.at, source, r.component, &delivery);
    (rebuilt == *r).then_some(CanonicalEvent {
        key,
        source,
        target: r.component,
        delivery,
    })
}

/// Checks that `records` is a well-formed trace prefix whose dispatch history ends at
/// `last` with execution digest `digest`.
pub(crate) fn check_prefix(
    records: &[TraceRecord],
    components: usize,
    last: Option<EventKey>,
    digest: &[u8; 32],
) -> Result<(), ResumeError> {
    let mut replayed = [0; 32];
    let mut current: Option<EventKey> = None;
    for (index, r) in records.iter().enumerate() {
        let malformed = ResumeError::MalformedRecord(index);
        if r.component.0 as usize >= components {
            return Err(malformed);
        }
        match r.origin {
            TraceOrigin::Runtime => {
                let ev = dispatched_event(r).ok_or(malformed)?;
                if current.is_some_and(|k| ev.key <= k) {
                    return Err(malformed);
                }
                replayed = chain(&replayed, &ev);
                current = Some(ev.key);
            }
            TraceOrigin::Component => {
                let in_place = match (r.at, current) {
                    (TraceAt::Init, None) => true,
                    (TraceAt::Event(k), Some(c)) => k == c,
                    _ => false,
                };
                if !in_place {
                    return Err(malformed);
                }
            }
        }
    }
    if current != last || replayed != *digest {
        return Err(ResumeError::HistoryMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use systemscope_contracts::event::Phase;
    use systemscope_contracts::time::Tick;

    use super::*;

    fn at() -> TraceAt {
        TraceAt::Event(EventKey {
            tick: Tick(3),
            phase: Phase::Transfer,
            sequence: 4,
        })
    }

    fn record(msg: mem_v1::MemMsg) -> TraceRecord {
        let delivery = Delivered::Message {
            port: PortId(2),
            msg: Message::MemV1(msg),
        };
        dispatch_record(at(), ComponentId(1), ComponentId(0), &delivery)
    }

    fn s(v: &str) -> Value {
        Value::Str(v.into())
    }

    fn fault() -> MemFault {
        MemFault::AccessFault
    }

    /// Every `mem.v1` variant and outcome, with the fields its dispatch record carries
    /// after `source`, `port`, `protocol` and `version`.
    fn cases() -> Vec<(mem_v1::MemMsg, Vec<(&'static str, Value)>)> {
        use mem_v1::MemMsg::*;
        vec![
            (
                ReadReq {
                    txn: TxnId(u64::MAX),
                    addr: u64::MAX,
                    len: 1,
                },
                vec![
                    ("msg", s("ReadReq")),
                    ("txn", Value::U64(u64::MAX)),
                    ("addr", Value::U64(u64::MAX)),
                    ("len", Value::U64(1)),
                ],
            ),
            (
                ReadResp {
                    txn: TxnId(0),
                    outcome: ReadOutcome::Data {
                        data: vec![0xDE, 0xAD],
                    },
                },
                vec![
                    ("msg", s("ReadResp")),
                    ("txn", Value::U64(0)),
                    ("outcome", s("Data")),
                    ("data", Value::Bytes(vec![0xDE, 0xAD])),
                ],
            ),
            (
                ReadResp {
                    txn: TxnId(7),
                    outcome: ReadOutcome::Fault { fault: fault() },
                },
                vec![
                    ("msg", s("ReadResp")),
                    ("txn", Value::U64(7)),
                    ("outcome", s("Fault")),
                    ("fault", s("AccessFault")),
                ],
            ),
            (
                WriteReq {
                    txn: TxnId(8),
                    addr: 0,
                    data: vec![0xAA],
                },
                vec![
                    ("msg", s("WriteReq")),
                    ("txn", Value::U64(8)),
                    ("addr", Value::U64(0)),
                    ("data", Value::Bytes(vec![0xAA])),
                ],
            ),
            (
                WriteResp {
                    txn: TxnId(8),
                    outcome: WriteOutcome::Done,
                },
                vec![
                    ("msg", s("WriteResp")),
                    ("txn", Value::U64(8)),
                    ("outcome", s("Done")),
                ],
            ),
            (
                WriteResp {
                    txn: TxnId(9),
                    outcome: WriteOutcome::Fault { fault: fault() },
                },
                vec![
                    ("msg", s("WriteResp")),
                    ("txn", Value::U64(9)),
                    ("outcome", s("Fault")),
                    ("fault", s("AccessFault")),
                ],
            ),
        ]
    }

    #[test]
    fn mem_v1_dispatch_fields_flatten_the_outcome() {
        for (msg, tail) in cases() {
            let mut fields = vec![
                ("source", Value::U64(1)),
                ("port", Value::U64(2)),
                ("protocol", s("mem")),
                ("version", Value::U64(1)),
            ];
            fields.extend(tail);
            assert_eq!(record(msg.clone()).fields, fields, "{msg:?}");
        }
    }

    #[test]
    fn mem_v1_dispatch_records_parse_back_to_their_event() {
        for (msg, _) in cases() {
            let ev = dispatched_event(&record(msg.clone())).expect("parses");
            assert_eq!(
                ev.delivery,
                Delivered::Message {
                    port: PortId(2),
                    msg: Message::MemV1(msg),
                }
            );
        }
    }

    #[test]
    fn altered_mem_v1_dispatch_records_are_rejected() {
        let fault = record(cases()[2].0.clone());
        let replace = |r: &TraceRecord, i: usize, v: Value| {
            let mut r = r.clone();
            r.fields[i].1 = v;
            dispatched_event(&r)
        };
        assert_eq!(replace(&fault, 6, s("Bogus")), None);
        assert_eq!(replace(&fault, 7, s("BusError")), None);
        // Version 0 names mem.v0, whose ReadResp has no outcome field.
        assert_eq!(replace(&fault, 3, Value::U64(0)), None);
        assert_eq!(replace(&fault, 3, Value::U64(2)), None);
        let done = record(cases()[4].0.clone());
        let mut extra = done.clone();
        extra.fields.push(("fault", s("AccessFault")));
        assert_eq!(dispatched_event(&extra), None);
        // A mem.v0 record relabelled as version 1 is not a mem.v1 record.
        let v0 = dispatch_record(
            at(),
            ComponentId(1),
            ComponentId(0),
            &Delivered::Message {
                port: PortId(2),
                msg: Message::Mem(MemMsg::WriteResp { txn: TxnId(8) }),
            },
        );
        assert!(dispatched_event(&v0).is_some());
        assert_eq!(replace(&v0, 3, Value::U64(1)), None);
    }

    fn m2_record(msg: Message) -> TraceRecord {
        let delivery = Delivered::Message {
            port: PortId(2),
            msg,
        };
        dispatch_record(at(), ComponentId(1), ComponentId(0), &delivery)
    }

    /// Every `irq.v0` and `block.v0` variant, outcome, and error, with the fields its
    /// dispatch record carries after `source` and `port`.
    fn m2_cases() -> Vec<(Message, Vec<(&'static str, Value)>)> {
        let irq = |asserted| {
            (
                Message::Irq(IrqMsg::Level { asserted }),
                vec![
                    ("protocol", s("irq")),
                    ("version", Value::U64(0)),
                    ("msg", s("Level")),
                    ("asserted", Value::Bool(asserted)),
                ],
            )
        };
        let block = |msg, tail: Vec<(&'static str, Value)>| {
            let mut fields = vec![("protocol", s("block")), ("version", Value::U64(0))];
            fields.extend(tail);
            (Message::Block(msg), fields)
        };
        let read_error = |error, name| {
            block(
                BlockMsg::ReadResult {
                    txn: TxnId(5),
                    outcome: BlockReadOutcome::Error { error },
                },
                vec![
                    ("msg", s("ReadResult")),
                    ("txn", Value::U64(5)),
                    ("outcome", s("Error")),
                    ("error", s(name)),
                ],
            )
        };
        let write_error = |error, name| {
            block(
                BlockMsg::WriteResult {
                    txn: TxnId(6),
                    outcome: BlockWriteOutcome::Error { error },
                },
                vec![
                    ("msg", s("WriteResult")),
                    ("txn", Value::U64(6)),
                    ("outcome", s("Error")),
                    ("error", s(name)),
                ],
            )
        };
        vec![
            irq(false),
            irq(true),
            block(
                BlockMsg::ReadBlock {
                    txn: TxnId(u64::MAX),
                    lba: u64::MAX,
                },
                vec![
                    ("msg", s("ReadBlock")),
                    ("txn", Value::U64(u64::MAX)),
                    ("lba", Value::U64(u64::MAX)),
                ],
            ),
            block(
                BlockMsg::WriteBlock {
                    txn: TxnId(0),
                    lba: 3,
                    data: vec![0xCA, 0xFE],
                },
                vec![
                    ("msg", s("WriteBlock")),
                    ("txn", Value::U64(0)),
                    ("lba", Value::U64(3)),
                    ("data", Value::Bytes(vec![0xCA, 0xFE])),
                ],
            ),
            block(
                BlockMsg::ReadResult {
                    txn: TxnId(5),
                    outcome: BlockReadOutcome::Data { data: vec![0xDE] },
                },
                vec![
                    ("msg", s("ReadResult")),
                    ("txn", Value::U64(5)),
                    ("outcome", s("Data")),
                    ("data", Value::Bytes(vec![0xDE])),
                ],
            ),
            read_error(MediaError::OutOfRange, "OutOfRange"),
            read_error(MediaError::BadBlock, "BadBlock"),
            block(
                BlockMsg::WriteResult {
                    txn: TxnId(6),
                    outcome: BlockWriteOutcome::Done,
                },
                vec![
                    ("msg", s("WriteResult")),
                    ("txn", Value::U64(6)),
                    ("outcome", s("Done")),
                ],
            ),
            write_error(MediaError::OutOfRange, "OutOfRange"),
            write_error(MediaError::BadBlock, "BadBlock"),
        ]
    }

    #[test]
    fn irq_and_block_dispatch_fields_flatten_the_outcome() {
        for (msg, tail) in m2_cases() {
            let mut fields = vec![("source", Value::U64(1)), ("port", Value::U64(2))];
            fields.extend(tail);
            assert_eq!(m2_record(msg.clone()).fields, fields, "{msg:?}");
        }
    }

    #[test]
    fn irq_and_block_dispatch_records_parse_back_to_their_event() {
        for (msg, _) in m2_cases() {
            let ev = dispatched_event(&m2_record(msg.clone())).expect("parses");
            assert_eq!(
                ev.delivery,
                Delivered::Message {
                    port: PortId(2),
                    msg
                }
            );
        }
    }

    #[test]
    fn altered_irq_and_block_dispatch_records_are_rejected() {
        let replace = |r: &TraceRecord, i: usize, v: Value| {
            let mut r = r.clone();
            r.fields[i].1 = v;
            dispatched_event(&r)
        };
        let level = m2_record(m2_cases()[1].0.clone());
        assert_eq!(replace(&level, 5, Value::U64(1)), None);
        assert_eq!(replace(&level, 4, s("Assert")), None);
        assert_eq!(replace(&level, 3, Value::U64(1)), None);
        assert_eq!(replace(&level, 2, s("mem")), None);
        let bad_block = m2_record(m2_cases()[6].0.clone());
        assert_eq!(replace(&bad_block, 7, s("Timeout")), None);
        assert_eq!(replace(&bad_block, 6, s("Fault")), None);
        assert_eq!(replace(&bad_block, 3, Value::U64(1)), None);
        assert_eq!(replace(&bad_block, 2, s("mem")), None);
        let done = m2_record(m2_cases()[7].0.clone());
        let mut extra = done.clone();
        extra.fields.push(("error", s("BadBlock")));
        assert_eq!(dispatched_event(&extra), None);
        // A mem.v1 record relabelled as block.v0 or irq.v0 is neither.
        let v0 = record(cases()[0].0.clone());
        assert_eq!(replace(&v0, 2, s("block")), None);
        assert_eq!(replace(&v0, 2, s("irq")), None);
    }
}
