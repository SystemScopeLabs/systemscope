//! The pure core (`docs/m3-design.md` §6.3, §17 M3.4a) against the independent oracle of
//! `common::oracle`: the accesses of the scripted operation and of the shutdown, their
//! splitting, the completion rules, and which `(op, step, data)` states exist.

mod common;

use common::layout::*;
use common::{Mem, oracle};
use proptest::prelude::*;
use systemscope_contracts::time::ClockDomainId;
use systemscope_os::KernelConfig;
use systemscope_os::core::{
    Access, Completion, CoreError, MAX_ACCESS, Op, Operation, PAGE, SCRIPT_BYTE, chunks,
};

fn config() -> KernelConfig {
    common::layout::config(ClockDomainId(0))
}

fn with_frame(frame: u32) -> KernelConfig {
    KernelConfig {
        trap_frame: frame,
        ..config()
    }
}

/// Runs the operation `value` starts on `mem`, returning its accesses as the oracle
/// writes them, and checking at every step that `from_parts` rebuilds the state.
fn drive(config: &KernelConfig, value: u32, mem: &mut Mem) -> Vec<oracle::Access> {
    let mut op = Operation::start(config, value);
    let mut out = Vec::new();
    while let Some(access) = op.access(config) {
        let rebuilt = Operation::from_parts(config, op.op(), op.step(), op.data().to_vec());
        assert_eq!(
            rebuilt.as_ref(),
            Some(&op),
            "from_parts at step {}",
            op.step()
        );
        let completion = match access {
            Access::Read { addr, len } => {
                let data: Vec<u8> = (addr..addr + u64::from(len))
                    .map(|a| mem.get(&a).copied().unwrap_or(0))
                    .collect();
                out.push((false, addr, vec![0; len as usize]));
                Completion::Data(data)
            }
            Access::Write { addr, data } => {
                // The memory is the RAM; the UART byte is only recorded.
                if (RAM_BASE..RAM_BASE + RAM_SIZE).contains(&addr) {
                    for (a, b) in (addr..).zip(&data) {
                        mem.insert(a, *b);
                    }
                }
                out.push((true, addr, data));
                Completion::Written
            }
        };
        op.complete(config, completion).unwrap();
    }
    assert!(op.is_finished(config));
    assert_eq!(op.step(), op.steps(config));
    assert!(op.data().is_empty(), "a finished operation keeps no data");
    out
}

fn frame_memory(frame: u32, words: &[u32]) -> Mem {
    let mut mem = Mem::new();
    for (i, w) in words.iter().enumerate() {
        for (k, b) in w.to_le_bytes().into_iter().enumerate() {
            mem.insert(u64::from(frame) + 4 * i as u64 + k as u64, b);
        }
    }
    mem
}

#[test]
fn the_scripted_operation_is_the_oracles() {
    let words: Vec<u32> = (0..38).map(|i| 0x0101_0101 * i).collect();
    let mut mem = frame_memory(TRAP_FRAME, &words);
    let mut want = mem.clone();
    let got = drive(&config(), TRAP_FRAME, &mut mem);
    let expected = oracle::script(u64::from(TRAP_FRAME), UART_BASE, &mut want);
    assert_eq!(got, expected);
    // Only sepc (word 31) changed in memory.
    let sepc = u64::from(TRAP_FRAME) + 0x7C;
    for (a, b) in &mem {
        if (sepc..sepc + 4).contains(a) {
            continue;
        }
        assert_eq!(*b, frame_memory(TRAP_FRAME, &words)[a]);
    }
    let bytes: Vec<u8> = (sepc..sepc + 4).map(|a| mem[&a]).collect();
    assert_eq!(u32::from_le_bytes(bytes.try_into().unwrap()), words[31] + 4);
    assert_eq!(got.last().unwrap(), &(true, UART_BASE, vec![SCRIPT_BYTE]));
    assert_eq!(Operation::start(&config(), TRAP_FRAME).op(), Op::Script);
    assert_eq!(Operation::start(&config(), TRAP_FRAME).steps(&config()), 21);
}

#[test]
fn sepc_wraps_like_a_32_bit_register() {
    let mut words = vec![0; 38];
    words[31] = 0xFFFF_FFFC;
    let mut mem = frame_memory(TRAP_FRAME, &words);
    drive(&config(), TRAP_FRAME, &mut mem);
    let sepc = u64::from(TRAP_FRAME) + 0x7C;
    assert!((sepc..sepc + 4).all(|a| mem[&a] == 0));
}

#[test]
fn any_other_value_shuts_down() {
    for value in [
        0,
        TRAP_FRAME - 4,
        TRAP_FRAME + 4,
        KGATE_BASE as u32,
        u32::MAX,
    ] {
        let mut mem = Mem::new();
        let mut want = Mem::new();
        let op = Operation::start(&config(), value);
        assert_eq!(op.op(), Op::Shutdown);
        assert_eq!(op.op().name(), "shutdown");
        let got = drive(&config(), value, &mut mem);
        assert_eq!(got, oracle::shutdown(u64::from(TRAP_FRAME), &mut want));
        assert_eq!(mem, want);
    }
}

#[test]
fn chunks_split_at_16_bytes_and_at_pages() {
    assert_eq!(chunks(0x1000, 0), vec![]);
    assert_eq!(chunks(0x1000, 16), vec![(0x1000, 16)]);
    assert_eq!(chunks(0x1000, 17), vec![(0x1000, 16), (0x1010, 1)]);
    assert_eq!(chunks(0x1FF8, 16), vec![(0x1FF8, 8), (0x2000, 8)]);
    assert_eq!(chunks(0x1FFF, 2), vec![(0x1FFF, 1), (0x2000, 1)]);
    // The layout's frame: nine 16-byte chunks and one of 8.
    let frame = chunks(u64::from(TRAP_FRAME), 0x98);
    assert_eq!(frame.len(), 10);
    assert!(frame[..9].iter().all(|c| c.1 == 16));
    assert_eq!(frame[9], (u64::from(TRAP_FRAME) + 0x90, 8));
}

#[test]
fn a_frame_across_a_page_splits_there() {
    // A frame starting 4 bytes before a page boundary: the first chunk is 4 bytes.
    let frame = 0x8000_1FFC;
    let config = with_frame(frame);
    config.validate().unwrap();
    let mut mem = frame_memory(frame, &[7; 38]);
    let mut want = mem.clone();
    let got = drive(&config, frame, &mut mem);
    assert_eq!(got, oracle::script(u64::from(frame), UART_BASE, &mut want));
    assert_eq!(got[0].1, u64::from(frame));
    assert_eq!(got[0].2.len(), 4);
    assert_eq!(got[1].1, 0x8000_2000);
    assert_eq!(Operation::start(&config, frame).steps(&config), 2 * 11 + 1);
}

#[test]
fn completions_must_match_the_access() {
    let config = config();
    let mut op = Operation::start(&config, TRAP_FRAME);
    let before = op.clone();
    assert_eq!(
        op.complete(&config, Completion::Written),
        Err(CoreError::WrongKind)
    );
    assert_eq!(
        op.complete(&config, Completion::Data(vec![0; 15])),
        Err(CoreError::DataLength)
    );
    assert_eq!(
        op.complete(&config, Completion::Data(vec![0; 17])),
        Err(CoreError::DataLength)
    );
    assert_eq!(op, before, "a refused completion changes nothing");
    // Through the reads to the first write.
    for _ in 0..10 {
        let Some(Access::Read { len, .. }) = op.access(&config) else {
            panic!("a read");
        };
        op.complete(&config, Completion::Data(vec![0; len as usize]))
            .unwrap();
    }
    let before = op.clone();
    assert_eq!(
        op.complete(&config, Completion::Data(vec![0; 16])),
        Err(CoreError::WrongKind)
    );
    assert_eq!(op, before);
    while !op.is_finished(&config) {
        op.complete(&config, Completion::Written).unwrap();
    }
    let before = op.clone();
    assert_eq!(op.access(&config), None);
    assert_eq!(
        op.complete(&config, Completion::Written),
        Err(CoreError::Finished)
    );
    assert_eq!(op, before);
}

#[test]
fn from_parts_rejects_unreachable_states() {
    let config = config();
    // Past the last access.
    assert_eq!(Operation::from_parts(&config, Op::Script, 21, vec![]), None);
    assert_eq!(
        Operation::from_parts(&config, Op::Shutdown, 1, vec![]),
        None
    );
    assert_eq!(
        Operation::from_parts(&config, Op::Script, u32::MAX, vec![]),
        None
    );
    // Working data of the wrong length for the step.
    assert_eq!(Operation::from_parts(&config, Op::Script, 0, vec![0]), None);
    assert_eq!(
        Operation::from_parts(&config, Op::Script, 1, vec![0; 15]),
        None
    );
    assert_eq!(
        Operation::from_parts(&config, Op::Script, 10, vec![0; 0x97]),
        None
    );
    assert_eq!(
        Operation::from_parts(&config, Op::Script, 20, vec![0; 0x98]),
        None
    );
    assert_eq!(
        Operation::from_parts(&config, Op::Shutdown, 0, vec![0]),
        None
    );
    // Reachable ones.
    assert!(Operation::from_parts(&config, Op::Script, 1, vec![0; 16]).is_some());
    assert!(Operation::from_parts(&config, Op::Script, 10, vec![0; 0x98]).is_some());
    assert!(Operation::from_parts(&config, Op::Script, 20, vec![]).is_some());
    assert!(Operation::from_parts(&config, Op::Shutdown, 0, vec![]).is_some());
}

#[test]
fn access_accessors() {
    let r = Access::Read { addr: 8, len: 4 };
    let w = Access::Write {
        addr: 9,
        data: vec![1, 2],
    };
    assert_eq!((r.addr(), r.len(), r.is_empty()), (8, 4, false));
    assert_eq!((w.addr(), w.len(), w.is_empty()), (9, 2, false));
    assert!(Access::Read { addr: 0, len: 0 }.is_empty());
    assert!(
        Access::Write {
            addr: 0,
            data: vec![]
        }
        .is_empty()
    );
    assert_eq!(Op::Script.name(), "script");
}

proptest! {
    /// Chunks cover the range exactly, in order, each non-empty, at most 16 bytes, and
    /// inside one page.
    #[test]
    fn chunks_tile_the_range(base in 0u64..1 << 40, len in 0u64..200) {
        let c = chunks(base, len);
        let mut at = base;
        for &(addr, n) in &c {
            prop_assert_eq!(addr, at);
            prop_assert!(n > 0 && n <= MAX_ACCESS);
            prop_assert_eq!(addr / PAGE, (addr + n - 1) / PAGE);
            at += n;
        }
        prop_assert_eq!(at, base + len);
        // Maximal: a chunk ends early only at a page boundary or the end.
        for w in c.windows(2) {
            prop_assert!(w[0].1 == MAX_ACCESS || w[1].0 % PAGE == 0);
        }
    }

    /// For any aligned frame in the RAM below staging and any contents, the core makes
    /// the oracle's accesses and leaves the oracle's memory.
    #[test]
    fn the_core_matches_the_oracle(
        index in 0u32..((STAGING - RAM_BASE - 0x98) as u32 / 4),
        words in proptest::collection::vec(any::<u32>(), 38),
        value_offset in prop_oneof![Just(0u32), any::<u32>()],
    ) {
        let frame = RAM_BASE as u32 + 4 * index;
        let config = with_frame(frame);
        prop_assert!(config.validate().is_ok());
        let value = frame.wrapping_add(value_offset);
        let mut mem = frame_memory(frame, &words);
        let mut want = mem.clone();
        let got = drive(&config, value, &mut mem);
        let expected = if value == frame {
            oracle::script(u64::from(frame), UART_BASE, &mut want)
        } else {
            oracle::shutdown(u64::from(frame), &mut want)
        };
        prop_assert_eq!(got, expected);
        prop_assert_eq!(mem, want);
    }

    /// `from_parts` accepts exactly the states a run reaches.
    #[test]
    fn from_parts_accepts_only_reachable_states(
        script in any::<bool>(),
        step in 0u32..30,
        len in 0usize..0xA0,
    ) {
        let config = config();
        let op = if script { Op::Script } else { Op::Shutdown };
        let reachable = match op {
            Op::Script if step < 10 => len == 16 * step as usize,
            Op::Script if step < 20 => len == 0x98,
            Op::Script => step == 20 && len == 0,
            Op::Shutdown => step == 0 && len == 0,
        };
        prop_assert_eq!(
            Operation::from_parts(&config, op, step, vec![0; len]).is_some(),
            reachable
        );
    }
}
