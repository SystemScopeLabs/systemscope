//! Recorded traces and their digest (`docs/m0-design.md` §8.1).
//!
//! A [`Trace`] is the header plus every record in emission order. Its canonical stream
//! encoding is the only source of truth: [`Trace::digest`] hashes that encoding, and the
//! exporters in [`crate::export`] are views of the same records.

use systemscope_contracts::component::{ComponentId, Delivered};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem::MemMsg;
use systemscope_contracts::trace::{
    DISPATCH_KIND, TraceAt, TraceHeader, TraceOrigin, TraceRecord, Value, encode_stream,
};

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
    let Message::Mem(mem) = msg;
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
