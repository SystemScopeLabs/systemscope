//! Load and store semantics, including precise misaligned and access-fault traps, against
//! hand-computed vectors and an independent interpreter (`docs/m1-design.md` §5.3, §6).
//!
//! The oracle below shares no code with `prepare_memory` or the completions. It works on
//! mathematical integers: the effective address is reduced modulo 2^32, alignment is a
//! remainder, loads assemble bytes as a sum of `b[i] * 256^i`, sign extension subtracts
//! `2^(8n)` from values at or above `2^(8n - 1)`, and stores slice `rs2` by division.
//! Instructions reach it as encoded words that go through `decode`.

use proptest::prelude::*;
use systemscope_contracts::protocol::mem_v1::{MemFault, MemMsg, ReadOutcome, TxnId, WriteOutcome};
use systemscope_rv32i::{
    ExecOutcome, Instr, LoadExtension, LoadOp, LoadPlan, MemWidth, MemoryCompletionError,
    MemoryPlan, MemoryPrep, NotMemory, PendingEffect, PendingTrap, Reg, RegWrite, StoreOp,
    StorePlan, TrapCause, complete_load, complete_memory, complete_store, decode, execute_alu,
    execute_control, prepare_memory,
};

const PC: u32 = 0x8000_0100;

fn x(i: u8) -> Reg {
    Reg::new(i).unwrap()
}

// ---------------------------------------------------------------------------------------
// Encodings.

/// Every memory instruction, by mnemonic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Lb,
    Lh,
    Lw,
    Lbu,
    Lhu,
    Sb,
    Sh,
    Sw,
}

const LOADS: [Kind; 5] = [Kind::Lb, Kind::Lh, Kind::Lw, Kind::Lbu, Kind::Lhu];
const STORES: [Kind; 3] = [Kind::Sb, Kind::Sh, Kind::Sw];
const ALL: [Kind; 8] = [
    Kind::Lb,
    Kind::Lh,
    Kind::Lw,
    Kind::Lbu,
    Kind::Lhu,
    Kind::Sb,
    Kind::Sh,
    Kind::Sw,
];

impl Kind {
    fn is_load(self) -> bool {
        LOADS.contains(&self)
    }

    /// Bytes accessed.
    fn n(self) -> u32 {
        match self {
            Kind::Lb | Kind::Lbu | Kind::Sb => 1,
            Kind::Lh | Kind::Lhu | Kind::Sh => 2,
            Kind::Lw | Kind::Sw => 4,
        }
    }

    fn signed(self) -> bool {
        matches!(self, Kind::Lb | Kind::Lh | Kind::Lw)
    }

    fn funct3(self) -> u32 {
        match self {
            Kind::Lb | Kind::Sb => 0,
            Kind::Lh | Kind::Sh => 1,
            Kind::Lw | Kind::Sw => 2,
            Kind::Lbu => 4,
            Kind::Lhu => 5,
        }
    }
}

/// The instruction word: I-type for loads (`rd` = `r`), S-type for stores (`rs2` = `r`).
fn encode(kind: Kind, r: u8, rs1: u8, imm: i32) -> u32 {
    assert!((-2048..2048).contains(&imm));
    let imm = (imm as u32) & 0xfff;
    let (r, rs1) = (u32::from(r), u32::from(rs1));
    if kind.is_load() {
        imm << 20 | rs1 << 15 | kind.funct3() << 12 | r << 7 | 0x03
    } else {
        (imm >> 5) << 25 | r << 20 | rs1 << 15 | kind.funct3() << 12 | (imm & 0x1f) << 7 | 0x23
    }
}

fn instr(kind: Kind, r: u8, rs1: u8, imm: i32) -> Instr {
    decode(encode(kind, r, rs1, imm)).unwrap()
}

// ---------------------------------------------------------------------------------------
// The oracle.

const MOD: i64 = 1 << 32;

fn wrap(v: i64) -> u32 {
    u32::try_from(v.rem_euclid(MOD)).unwrap()
}

/// What a memory instruction does, in the oracle's own terms.
#[derive(Clone, Debug, PartialEq)]
enum Oracle {
    Misaligned { addr: u32 },
    Load { rd: u8, addr: u32 },
    Store { addr: u32, bytes: Vec<u8> },
}

fn effective(rs1: u32, imm: i32) -> u32 {
    wrap(i64::from(rs1) + i64::from(imm))
}

fn oracle(kind: Kind, r: u8, rs1: u32, rs2: u32, imm: i32) -> Oracle {
    let addr = effective(rs1, imm);
    if i64::from(addr) % i64::from(kind.n()) != 0 {
        Oracle::Misaligned { addr }
    } else if kind.is_load() {
        Oracle::Load { rd: r, addr }
    } else {
        let bytes = (0..kind.n())
            .map(|i| u8::try_from((i64::from(rs2) / 256i64.pow(i)) % 256).unwrap())
            .collect();
        Oracle::Store { addr, bytes }
    }
}

/// The register value a load of `bytes` produces.
fn loaded(kind: Kind, bytes: &[u8]) -> u32 {
    let v: i64 = bytes
        .iter()
        .enumerate()
        .map(|(i, &b)| i64::from(b) * 256i64.pow(u32::try_from(i).unwrap()))
        .sum();
    let bits = 8 * kind.n();
    if kind.signed() && v >= 1 << (bits - 1) {
        wrap(v - (1 << bits))
    } else {
        wrap(v)
    }
}

/// The oracle's expectation in the production types, for everything prepare returns.
fn expected_prep(kind: Kind, r: u8, rs1: u32, rs2: u32, imm: i32, pc: u32) -> MemoryPrep {
    let next_pc = wrap(i64::from(pc) + 4);
    match oracle(kind, r, rs1, rs2, imm) {
        Oracle::Misaligned { addr } => MemoryPrep::Trap(PendingTrap {
            cause: if kind.is_load() {
                TrapCause::LoadAddressMisaligned
            } else {
                TrapCause::StoreAddressMisaligned
            },
            tval: addr,
        }),
        Oracle::Load { rd, addr } => MemoryPrep::Request(MemoryPlan::Load(LoadPlan {
            rd: x(rd),
            addr,
            width: width(kind),
            extension: if kind.signed() {
                LoadExtension::Signed
            } else {
                LoadExtension::Unsigned
            },
            next_pc,
        })),
        Oracle::Store { addr, bytes } => MemoryPrep::Request(MemoryPlan::Store(StorePlan {
            addr,
            data: bytes,
            next_pc,
        })),
    }
}

fn width(kind: Kind) -> MemWidth {
    match kind.n() {
        1 => MemWidth::Byte,
        2 => MemWidth::Half,
        _ => MemWidth::Word,
    }
}

// ---------------------------------------------------------------------------------------
// Helpers for the targeted tests.

fn prep(kind: Kind, r: u8, rs1: u32, rs2: u32, imm: i32) -> MemoryPrep {
    prepare_memory(&instr(kind, r, 1, imm), PC, rs1, rs2).unwrap()
}

fn load_plan(kind: Kind, rd: u8, rs1: u32, imm: i32) -> LoadPlan {
    match prep(kind, rd, rs1, 0, imm) {
        MemoryPrep::Request(MemoryPlan::Load(plan)) => plan,
        other => panic!("expected a load plan, got {other:?}"),
    }
}

fn store_plan(kind: Kind, rs1: u32, rs2: u32, imm: i32) -> StorePlan {
    match prep(kind, 2, rs1, rs2, imm) {
        MemoryPrep::Request(MemoryPlan::Store(plan)) => plan,
        other => panic!("expected a store plan, got {other:?}"),
    }
}

/// The effective address `prepare_memory` computes, trap or not.
fn address(kind: Kind, rs1: u32, imm: i32) -> u32 {
    match prep(kind, 5, rs1, 0, imm) {
        MemoryPrep::Request(MemoryPlan::Load(p)) => p.addr,
        MemoryPrep::Request(MemoryPlan::Store(p)) => p.addr,
        MemoryPrep::Trap(t) => t.tval,
    }
}

fn is_request(kind: Kind, addr: u32) -> bool {
    matches!(prep(kind, 5, addr, 0, 0), MemoryPrep::Request(_))
}

fn data(bytes: &[u8]) -> ReadOutcome {
    ReadOutcome::Data {
        data: bytes.to_vec(),
    }
}

const READ_FAULT: ReadOutcome = ReadOutcome::Fault {
    fault: MemFault::AccessFault,
};
const WRITE_FAULT: WriteOutcome = WriteOutcome::Fault {
    fault: MemFault::AccessFault,
};

/// The value a load of `kind` writes when memory returns `bytes`.
fn load_value(kind: Kind, bytes: &[u8]) -> u32 {
    let plan = load_plan(kind, 7, 0x1000, 0);
    match complete_load(&plan, &data(bytes)).unwrap() {
        ExecOutcome::Effect(PendingEffect {
            reg_write: Some(RegWrite { rd, value }),
            next_pc,
        }) => {
            assert_eq!((rd, next_pc), (x(7), PC + 4));
            value
        }
        other => panic!("expected a register write, got {other:?}"),
    }
}

fn trap(cause: TrapCause, tval: u32) -> ExecOutcome {
    ExecOutcome::Trap(PendingTrap { cause, tval })
}

// ---------------------------------------------------------------------------------------
// Effective addresses.

#[test]
fn effective_address_is_rs1_plus_the_sign_extended_offset() {
    for kind in ALL {
        assert_eq!(address(kind, 0x1000, 0), 0x1000, "{kind:?} zero");
        assert_eq!(address(kind, 0x1000, 8), 0x1008, "{kind:?} positive");
        assert_eq!(address(kind, 0x1000, 2044), 0x17fc, "{kind:?} max");
        assert_eq!(address(kind, 0x1000, -8), 0x0ff8, "{kind:?} negative");
        assert_eq!(address(kind, 0x1000, -2048), 0x0800, "{kind:?} min");
    }
}

#[test]
fn effective_address_wraps_without_faulting() {
    for kind in ALL {
        // Past the top, and below zero.
        assert_eq!(address(kind, 0xffff_fffc, 4), 0, "{kind:?}");
        assert_eq!(address(kind, 0xffff_fffc, 8), 4, "{kind:?}");
        assert_eq!(address(kind, 0, -4), 0xffff_fffc, "{kind:?}");
        assert_eq!(address(kind, 4, -2048), 0xffff_f804, "{kind:?}");
        // Wrapped, aligned addresses are requests: mapping is the bus's business.
        assert!(matches!(
            prep(kind, 5, 0xffff_fffc, 0, 4),
            MemoryPrep::Request(_)
        ));
    }
}

#[test]
fn effective_address_reaches_both_ends_of_the_address_space() {
    for kind in ALL {
        assert_eq!(address(kind, 0, 0), 0, "{kind:?}");
        assert_eq!(address(kind, 4, -4), 0, "{kind:?}");
        assert_eq!(address(kind, u32::MAX, 0), u32::MAX, "{kind:?}");
        assert_eq!(address(kind, 0, -1), u32::MAX, "{kind:?}");
    }
    // A byte at the very last address is an ordinary aligned access.
    assert_eq!(load_plan(Kind::Lbu, 3, 0, -1).addr, u32::MAX);
    assert_eq!(store_plan(Kind::Sb, u32::MAX, 0xab, 0).addr, u32::MAX);
}

#[test]
fn the_base_register_is_rs1_and_the_plan_continues_at_pc_plus_4() {
    let plan = load_plan(Kind::Lw, 3, 0x2000, 4);
    assert_eq!(plan.next_pc, PC + 4);
    // rs2 plays no part in a load.
    let with = |rs2| prepare_memory(&instr(Kind::Lw, 3, 1, 4), PC, 0x2000, rs2);
    assert_eq!(with(0), with(u32::MAX));
    // next_pc wraps like every other address.
    let wrapped = prepare_memory(&instr(Kind::Sw, 2, 1, 0), 0xffff_fffc, 0x10, 0).unwrap();
    let MemoryPrep::Request(MemoryPlan::Store(plan)) = wrapped else {
        panic!("expected a store plan");
    };
    assert_eq!(plan.next_pc, 0);
}

// ---------------------------------------------------------------------------------------
// Alignment.

#[test]
fn byte_accesses_are_always_aligned() {
    for kind in [Kind::Lb, Kind::Lbu, Kind::Sb] {
        for addr in [0x1000, 0x1001, 0x1002, 0x1003, 0x1005, 0x1007, u32::MAX] {
            assert!(is_request(kind, addr), "{kind:?} {addr:#x}");
        }
    }
}

#[test]
fn halfword_accesses_must_be_2_byte_aligned() {
    for kind in [Kind::Lh, Kind::Lhu, Kind::Sh] {
        for low in 0..4u32 {
            let addr = 0x1000 | low;
            assert_eq!(is_request(kind, addr), low % 2 == 0, "{kind:?} {addr:#x}");
        }
        assert!(is_request(kind, 0xffff_fffe));
        assert!(!is_request(kind, u32::MAX));
    }
}

#[test]
fn word_accesses_must_be_4_byte_aligned() {
    for kind in [Kind::Lw, Kind::Sw] {
        for low in 0..8u32 {
            let addr = 0x1000 | low;
            assert_eq!(is_request(kind, addr), low % 4 == 0, "{kind:?} {addr:#x}");
        }
        assert!(is_request(kind, 0xffff_fffc));
        assert!(!is_request(kind, 0xffff_fffe));
    }
}

#[test]
fn misaligned_loads_trap_with_the_effective_address() {
    for (kind, rs1, imm) in [
        (Kind::Lh, 0x1000u32, 1),
        (Kind::Lhu, 0x1001, 0),
        (Kind::Lh, 0x1004, -1),
        (Kind::Lw, 0x1000, 2),
        (Kind::Lw, 0x1001, 0),
        (Kind::Lw, 0x1000, -1),
        (Kind::Lw, 0xffff_fffe, 3),
    ] {
        let addr = rs1.wrapping_add(imm as u32);
        assert_eq!(
            prep(kind, 5, rs1, 0, imm),
            MemoryPrep::Trap(PendingTrap {
                cause: TrapCause::LoadAddressMisaligned,
                tval: addr,
            }),
            "{kind:?} {addr:#x}"
        );
    }
}

#[test]
fn misaligned_stores_trap_before_any_request() {
    for (kind, addr) in [
        (Kind::Sh, 0x1001),
        (Kind::Sh, 0x1003),
        (Kind::Sw, 0x1001),
        (Kind::Sw, 0x1002),
        (Kind::Sw, 0x1003),
        (Kind::Sw, u32::MAX),
    ] {
        // No plan exists, so there are no bytes to write.
        assert_eq!(
            prep(kind, 2, addr, 0xdead_beef, 0),
            MemoryPrep::Trap(PendingTrap {
                cause: TrapCause::StoreAddressMisaligned,
                tval: addr,
            }),
            "{kind:?} {addr:#x}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Loads.

#[test]
fn lb_sign_extends_and_lbu_zero_extends() {
    for (byte, lb, lbu) in [
        (0x00, 0x0000_0000, 0x0000_0000),
        (0x01, 0x0000_0001, 0x0000_0001),
        (0x7f, 0x0000_007f, 0x0000_007f),
        (0x80, 0xffff_ff80, 0x0000_0080),
        (0xfe, 0xffff_fffe, 0x0000_00fe),
        (0xff, 0xffff_ffff, 0x0000_00ff),
    ] {
        assert_eq!(load_value(Kind::Lb, &[byte]), lb, "LB {byte:#04x}");
        assert_eq!(load_value(Kind::Lbu, &[byte]), lbu, "LBU {byte:#04x}");
    }
}

#[test]
fn lh_sign_extends_and_lhu_zero_extends() {
    for (bytes, lh, lhu) in [
        ([0x00, 0x00], 0x0000_0000, 0x0000_0000),
        ([0xff, 0x7f], 0x0000_7fff, 0x0000_7fff),
        ([0x00, 0x80], 0xffff_8000, 0x0000_8000),
        ([0xff, 0xff], 0xffff_ffff, 0x0000_ffff),
        // The sign is bit 15, not bit 7.
        ([0x80, 0x00], 0x0000_0080, 0x0000_0080),
    ] {
        assert_eq!(load_value(Kind::Lh, &bytes), lh, "LH {bytes:02x?}");
        assert_eq!(load_value(Kind::Lhu, &bytes), lhu, "LHU {bytes:02x?}");
    }
}

#[test]
fn lw_loads_the_word_unchanged() {
    for (bytes, value) in [
        ([0x00, 0x00, 0x00, 0x00], 0x0000_0000),
        ([0xff, 0xff, 0xff, 0x7f], 0x7fff_ffff),
        ([0x00, 0x00, 0x00, 0x80], 0x8000_0000),
        ([0xff, 0xff, 0xff, 0xff], 0xffff_ffff),
    ] {
        assert_eq!(load_value(Kind::Lw, &bytes), value, "LW {bytes:02x?}");
    }
}

#[test]
fn loads_are_little_endian() {
    assert_eq!(load_value(Kind::Lw, &[0x78, 0x56, 0x34, 0x12]), 0x1234_5678);
    assert_eq!(load_value(Kind::Lhu, &[0x34, 0x12]), 0x1234);
    assert_eq!(load_value(Kind::Lh, &[0x34, 0x92]), 0xffff_9234);
}

#[test]
fn a_load_writes_nothing_until_its_response() {
    let plan = load_plan(Kind::Lw, 9, 0x1000, 0);
    assert_eq!(
        plan,
        LoadPlan {
            rd: x(9),
            addr: 0x1000,
            width: MemWidth::Word,
            extension: LoadExtension::Signed,
            next_pc: PC + 4,
        }
    );
    assert_eq!(width(Kind::Lw).bytes(), 4);
    assert_eq!(MemWidth::Byte.bytes(), 1);
    assert_eq!(MemWidth::Half.bytes(), 2);
}

/// A load to `x0` is still performed: it keeps its request, can fault, and completes with
/// a write to `x0` that the register file discards.
#[test]
fn loads_to_x0_still_access_memory() {
    let plan = load_plan(Kind::Lw, 0, 0x1000, 0);
    assert_eq!(plan.rd, Reg::ZERO);
    assert_eq!(
        complete_load(&plan, &data(&[1, 2, 3, 4])),
        Ok(ExecOutcome::Effect(PendingEffect {
            reg_write: Some(RegWrite {
                rd: Reg::ZERO,
                value: 0x0403_0201,
            }),
            next_pc: PC + 4,
        }))
    );
    assert_eq!(
        complete_load(&plan, &READ_FAULT),
        Ok(trap(TrapCause::LoadAccessFault, 0x1000))
    );
    // Misaligned loads to x0 trap too.
    assert!(matches!(
        prep(Kind::Lw, 0, 0x1002, 0, 0),
        MemoryPrep::Trap(_)
    ));
}

// ---------------------------------------------------------------------------------------
// Stores.

#[test]
fn stores_send_the_low_bytes_of_rs2_little_endian() {
    for (rs2, sb, sh, sw) in [
        (0x1234_5678, [0x78], [0x78, 0x56], [0x78, 0x56, 0x34, 0x12]),
        (u32::MAX, [0xff], [0xff, 0xff], [0xff; 4]),
        (0, [0x00], [0x00, 0x00], [0x00; 4]),
        (0x8000_0001, [0x01], [0x01, 0x00], [0x01, 0x00, 0x00, 0x80]),
    ] {
        assert_eq!(store_plan(Kind::Sb, 0x1000, rs2, 0).data, sb, "SB {rs2:#x}");
        assert_eq!(store_plan(Kind::Sh, 0x1000, rs2, 0).data, sh, "SH {rs2:#x}");
        assert_eq!(store_plan(Kind::Sw, 0x1000, rs2, 0).data, sw, "SW {rs2:#x}");
    }
}

#[test]
fn a_store_plan_keeps_address_data_and_next_pc() {
    assert_eq!(
        store_plan(Kind::Sh, 0x2000, 0xcafe_babe, -2),
        StorePlan {
            addr: 0x1ffe,
            data: vec![0xbe, 0xba],
            next_pc: PC + 4,
        }
    );
}

#[test]
fn completed_stores_retire_without_a_register_write() {
    for kind in STORES {
        let plan = store_plan(kind, 0x1000, 0x55, 0);
        assert_eq!(
            complete_store(&plan, &WriteOutcome::Done),
            ExecOutcome::Effect(PendingEffect {
                reg_write: None,
                next_pc: PC + 4,
            }),
            "{kind:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Access faults.

#[test]
fn load_access_faults_trap_with_the_effective_address() {
    for kind in LOADS {
        let plan = load_plan(kind, 4, 0x4000_0000, -4 * i32::try_from(kind.n()).unwrap());
        assert_eq!(
            complete_load(&plan, &READ_FAULT),
            Ok(trap(TrapCause::LoadAccessFault, plan.addr)),
            "{kind:?}"
        );
        assert_eq!(plan.addr, 0x4000_0000 - 4 * kind.n());
    }
}

#[test]
fn store_access_faults_trap_with_the_effective_address() {
    for kind in STORES {
        let plan = store_plan(kind, 0xffff_fff0, 7, 8);
        assert_eq!(
            complete_store(&plan, &WRITE_FAULT),
            trap(TrapCause::StoreAccessFault, 0xffff_fff8),
            "{kind:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Malformed responses.

#[test]
fn data_of_the_wrong_length_is_a_model_bug_not_a_trap() {
    for (kind, lengths) in [
        (Kind::Lb, &[0, 2, 4][..]),
        (Kind::Lbu, &[0, 2]),
        (Kind::Lh, &[0, 1, 3, 4]),
        (Kind::Lhu, &[1, 3]),
        (Kind::Lw, &[0, 1, 2, 3, 5, 8]),
    ] {
        let plan = load_plan(kind, 1, 0x1000, 0);
        for &len in lengths {
            assert_eq!(
                complete_load(&plan, &data(&vec![0x80; len])),
                Err(MemoryCompletionError::DataLength {
                    expected: kind.n(),
                    actual: len,
                }),
                "{kind:?} with {len} bytes"
            );
        }
    }
}

#[test]
fn responses_of_the_wrong_kind_are_model_bugs() {
    let load = MemoryPlan::Load(load_plan(Kind::Lw, 1, 0x1000, 0));
    let store = MemoryPlan::Store(store_plan(Kind::Sw, 0x1000, 1, 0));
    let read_resp = MemMsg::ReadResp {
        txn: TxnId(3),
        outcome: data(&[1, 2, 3, 4]),
    };
    let write_resp = MemMsg::WriteResp {
        txn: TxnId(3),
        outcome: WriteOutcome::Done,
    };
    let read_req = MemMsg::ReadReq {
        txn: TxnId(3),
        addr: 0x1000,
        len: 4,
    };
    let write_req = MemMsg::WriteReq {
        txn: TxnId(3),
        addr: 0x1000,
        data: vec![1, 0, 0, 0],
    };
    let kind = Err(MemoryCompletionError::ResponseKind);
    for msg in [&write_resp, &read_req, &write_req] {
        assert_eq!(complete_memory(&load, msg), kind, "load with {msg:?}");
    }
    for msg in [&read_resp, &read_req, &write_req] {
        assert_eq!(complete_memory(&store, msg), kind, "store with {msg:?}");
    }
    // Matching pairs complete as the typed functions do.
    assert_eq!(
        complete_memory(&load, &read_resp),
        Ok(ExecOutcome::Effect(PendingEffect {
            reg_write: Some(RegWrite {
                rd: x(1),
                value: 0x0403_0201,
            }),
            next_pc: PC + 4,
        }))
    );
    assert_eq!(
        complete_memory(&store, &write_resp),
        Ok(ExecOutcome::Effect(PendingEffect {
            reg_write: None,
            next_pc: PC + 4,
        }))
    );
    // Faults stay architectural through the unified entry point, and length errors stay
    // model bugs.
    let fault = MemMsg::ReadResp {
        txn: TxnId(0),
        outcome: READ_FAULT,
    };
    assert_eq!(
        complete_memory(&load, &fault),
        Ok(trap(TrapCause::LoadAccessFault, 0x1000))
    );
    let short = MemMsg::ReadResp {
        txn: TxnId(0),
        outcome: data(&[1]),
    };
    assert_eq!(
        complete_memory(&load, &short),
        Err(MemoryCompletionError::DataLength {
            expected: 4,
            actual: 1
        })
    );
}

#[test]
fn completion_errors_explain_themselves() {
    let e = MemoryCompletionError::DataLength {
        expected: 2,
        actual: 3,
    };
    assert_eq!(e.to_string(), "load of 2 bytes got 3 bytes of data");
    assert_eq!(
        MemoryCompletionError::ResponseKind.to_string(),
        "memory response of the wrong kind for the request"
    );
}

// ---------------------------------------------------------------------------------------
// Everything else.

#[test]
fn other_instructions_are_not_memory() {
    // LUI, AUIPC, JAL, JALR, BEQ, ADDI, SLLI, ADD, FENCE, ECALL, EBREAK.
    for word in [
        0x1234_52b7,
        0x0000_1297,
        0x0080_00ef,
        0x0000_8067,
        0x0000_0463,
        0x0010_0293,
        0x0013_1293,
        0x0073_02b3,
        0x0ff0_000f,
        0x0000_0073,
        0x0010_0073,
    ] {
        let instr = decode(word).unwrap();
        assert_eq!(
            prepare_memory(&instr, PC, 1, 2),
            Err(NotMemory),
            "{word:#010x}"
        );
    }
}

#[test]
fn the_encoder_matches_the_decoder() {
    assert_eq!(
        instr(Kind::Lhu, 5, 6, -3),
        Instr::Load {
            op: LoadOp::Hu,
            rd: x(5),
            rs1: x(6),
            offset: -3,
        }
    );
    assert_eq!(
        instr(Kind::Sw, 7, 8, -2048),
        Instr::Store {
            op: StoreOp::W,
            rs1: x(8),
            rs2: x(7),
            offset: -2048,
        }
    );
    // `sw x7, 2047(x8)` and `lb x1, 0(x2)`, assembled by hand.
    assert_eq!(encode(Kind::Sw, 7, 8, 2047), 0x7e74_2fa3);
    assert_eq!(encode(Kind::Lb, 1, 2, 0), 0x0001_0083);
}

// ---------------------------------------------------------------------------------------
// Properties.

/// Addresses and operands, biased toward the boundaries.
fn value() -> impl Strategy<Value = u32> {
    prop_oneof![
        2 => any::<u32>(),
        1 => prop::sample::select(vec![
            0, 1, 2, 3, 4, 0x7fff, 0x8000, 0xffff, 0x7fff_ffff, 0x8000_0000, 0xffff_fffc,
            0xffff_ffff,
        ]),
    ]
}

fn offset() -> impl Strategy<Value = i32> {
    prop_oneof![
        2 => -2048i32..2048,
        1 => prop::sample::select(vec![0, 1, 2, 3, 4, -1, -2, -3, -4, 2047, -2048]),
    ]
}

fn kind() -> impl Strategy<Value = Kind> {
    prop::sample::select(ALL.to_vec())
}

fn load_kind() -> impl Strategy<Value = Kind> {
    prop::sample::select(LOADS.to_vec())
}

/// Response bytes, biased toward sign boundaries.
fn byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        2 => any::<u8>(),
        1 => prop::sample::select(vec![0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    /// From the encoded word through prepare and completion, every memory instruction
    /// does what the oracle says, for data and fault responses alike.
    #[test]
    fn memory_instructions_match_the_oracle(
        kind in kind(),
        r in 0u8..32,
        base in 0u8..32,
        imm in offset(),
        pc in value(),
        rs1 in value(),
        rs2 in value(),
        bytes in prop::collection::vec(byte(), 4),
    ) {
        let instr = instr(kind, r, base, imm);
        let prep = prepare_memory(&instr, pc, rs1, rs2).unwrap();
        prop_assert_eq!(&prep, &expected_prep(kind, r, rs1, rs2, imm, pc));
        let next_pc = wrap(i64::from(pc) + 4);
        match (oracle(kind, r, rs1, rs2, imm), prep) {
            (Oracle::Misaligned { .. }, MemoryPrep::Trap(_)) => {}
            (Oracle::Load { rd, addr }, MemoryPrep::Request(MemoryPlan::Load(plan))) => {
                let n = kind.n() as usize;
                prop_assert_eq!(
                    complete_load(&plan, &data(&bytes[..n])),
                    Ok(ExecOutcome::Effect(PendingEffect {
                        reg_write: Some(RegWrite { rd: x(rd), value: loaded(kind, &bytes[..n]) }),
                        next_pc,
                    }))
                );
                prop_assert_eq!(
                    complete_load(&plan, &READ_FAULT),
                    Ok(trap(TrapCause::LoadAccessFault, addr))
                );
            }
            (Oracle::Store { addr, .. }, MemoryPrep::Request(MemoryPlan::Store(plan))) => {
                prop_assert_eq!(
                    complete_store(&plan, &WriteOutcome::Done),
                    ExecOutcome::Effect(PendingEffect { reg_write: None, next_pc })
                );
                prop_assert_eq!(
                    complete_store(&plan, &WRITE_FAULT),
                    trap(TrapCause::StoreAccessFault, addr)
                );
            }
            (o, p) => prop_assert!(false, "oracle {:?} but prepared {:?}", o, p),
        }
    }

    /// Any decoded word is a memory instruction exactly when it is a load or store, and
    /// never also an ALU or control instruction.
    #[test]
    fn only_loads_and_stores_are_memory(word in any::<u32>(), a in value(), b in value()) {
        if let Ok(instr) = decode(word) {
            let memory = prepare_memory(&instr, PC, a, b);
            let is_memory = matches!(instr, Instr::Load { .. } | Instr::Store { .. });
            prop_assert_eq!(memory.is_ok(), is_memory);
            if is_memory {
                prop_assert!(execute_alu(&instr, PC, a, b).is_err());
                prop_assert!(execute_control(&instr, PC, a, b).is_err());
            }
        }
    }

    #[test]
    fn the_effective_address_is_rs1_plus_offset_mod_2_32(
        kind in kind(),
        rs1 in value(),
        imm in offset(),
    ) {
        prop_assert_eq!(address(kind, rs1, imm), effective(rs1, imm));
    }

    /// Signed and unsigned loads agree below the sign bit and differ by the extension
    /// above it.
    #[test]
    fn signed_and_unsigned_loads_agree_below_the_sign_bit(b0 in byte(), b1 in byte()) {
        let (lb, lbu) = (load_value(Kind::Lb, &[b0]), load_value(Kind::Lbu, &[b0]));
        if b0 < 0x80 {
            prop_assert_eq!(lb, lbu);
        } else {
            prop_assert_eq!(lb, lbu | 0xffff_ff00);
        }
        let (lh, lhu) = (load_value(Kind::Lh, &[b0, b1]), load_value(Kind::Lhu, &[b0, b1]));
        if b1 < 0x80 {
            prop_assert_eq!(lh, lhu);
        } else {
            prop_assert_eq!(lh, lhu | 0xffff_0000);
        }
    }

    /// A word stored and loaded back through the same byte order is unchanged.
    #[test]
    fn words_round_trip_through_store_and_load(v in value()) {
        let stored = store_plan(Kind::Sw, 0x1000, v, 0).data;
        prop_assert_eq!(load_value(Kind::Lw, &stored), v);
        let half = store_plan(Kind::Sh, 0x1000, v, 0).data;
        prop_assert_eq!(load_value(Kind::Lhu, &half), v % 0x1_0000);
        let byte = store_plan(Kind::Sb, 0x1000, v, 0).data;
        prop_assert_eq!(load_value(Kind::Lbu, &byte), v % 0x100);
    }

    /// Any other amount of data is a model bug, reported with both lengths.
    #[test]
    fn wrong_length_data_is_always_an_error(kind in load_kind(), skip in 0usize..8) {
        // Every length in 0..=8 but the right one.
        let n = kind.n() as usize;
        let len = if skip < n { skip } else { skip + 1 };
        let plan = load_plan(kind, 1, 0x1000, 0);
        prop_assert_eq!(
            complete_load(&plan, &data(&vec![0xa5; len])),
            Err(MemoryCompletionError::DataLength { expected: kind.n(), actual: len })
        );
    }
}
