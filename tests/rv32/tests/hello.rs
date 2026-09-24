//! M1-A5: `hello.elf` runs through the loader, the RAM, the CPU, the bus, and the UART,
//! and prints exactly `Hello, SystemScope!\n` (`docs/m1-design.md` §7.3, §9).
//!
//! Everything here runs the committed ELF on the real `m1-reference` platform: the output
//! is read back from the UART and from its trace records, never from the ELF.

use std::cell::Cell;
use std::fs;
use std::rc::Rc;

use systemscope_contracts::component::Delivered;
use systemscope_contracts::observe::{Control, EventView, Observer, WorldView};
use systemscope_contracts::protocol::Message;
use systemscope_contracts::protocol::mem_v1::MemMsg;
use systemscope_elf::LoadImage;
use systemscope_platform::uart;
use systemscope_runtime::runtime::Dispatched;
use systemscope_rv32::hello::{
    self, EXPECTED_OUTPUT, HELLO_SCRIPT, HelloManifest, HelloRun, judge, uart_traffic,
};
use systemscope_rv32::runner::{self, BUS, CPU, End, PASS_CAUSE, Start, UART};
use systemscope_rv32::{BUILD_SCRIPT, FLAGS, RAM_BASE, TOOLCHAIN, UART_BASE, workspace_root};
use systemscope_rv32i::{Instr, StoreOp, decode};

fn image() -> LoadImage {
    let root = workspace_root();
    HelloManifest::read(&root)
        .and_then(|m| m.read_image(&root))
        .unwrap()
}

fn fresh(image: &LoadImage, traced: bool) -> HelloRun {
    hello::run(image, Start::Init { traced }, Vec::new())
}

fn mem(ev: &Dispatched) -> Option<&MemMsg> {
    match &ev.delivery {
        Delivered::Message {
            msg: Message::MemV1(msg),
            ..
        } => Some(msg),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// The fixture.

#[test]
fn the_committed_hello_elf_matches_its_manifest() {
    let manifest = hello::verify(&workspace_root()).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(manifest.entry, RAM_BASE);
    assert_eq!(manifest.image_hash, manifest.blake3);
    assert_eq!(manifest.flags, FLAGS);
}

/// The value of `NAME=value` or `NAME='value'` in a build script.
fn script_value<'a>(script: &'a str, name: &str) -> &'a str {
    script
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name}"))
        .trim_matches('\'')
}

fn script_flags(script: &str) -> Vec<&str> {
    script
        .split_once("FLAGS=(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(flags, _)| flags.split_whitespace().collect())
        .expect("a FLAGS array")
}

/// hello.elf reuses the rv32ui toolchain pins and flags, and both scripts agree.
#[test]
fn the_hello_build_reuses_the_rv32ui_pins() {
    let root = workspace_root();
    let hello = fs::read_to_string(root.join(HELLO_SCRIPT)).unwrap();
    let rv32ui = fs::read_to_string(root.join(BUILD_SCRIPT)).unwrap();
    for script in [&hello, &rv32ui] {
        assert_eq!(
            script_value(script, "GCC_VERSION"),
            TOOLCHAIN[0].version_line
        );
        assert_eq!(
            script_value(script, "AS_VERSION"),
            TOOLCHAIN[1].version_line
        );
        assert_eq!(script_flags(script), FLAGS);
    }
    // Fixed object names and the fixture mode, the two M1.6 reproducibility lessons.
    assert!(hello.contains("-c hello.S -o \"$dir/build/hello.o\""));
    assert!(hello.contains("install -m 0644"));
}

#[test]
fn the_expected_output_is_twenty_bytes_without_a_terminator() {
    assert_eq!(
        EXPECTED_OUTPUT,
        &[
            0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20, 0x53, 0x79, 0x73, 0x74, 0x65, 0x6d, 0x53,
            0x63, 0x6f, 0x70, 0x65, 0x21, 0x0a,
        ]
    );
    assert_eq!(EXPECTED_OUTPUT.len(), 20);
    assert!(!EXPECTED_OUTPUT.contains(&0));
}

/// The code, decoded with SystemScope's own decoder: RV32I only, one store, an `SB`, and
/// one `ECALL`, the only SYSTEM instruction.
#[test]
fn the_code_stores_single_bytes_and_ends_with_one_ecall() {
    let image = image();
    let code = image.segments.iter().find(|s| s.offset == 0).unwrap();
    let words: Vec<Instr> = code.bytes[..0x30]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| decode(u32::from_le_bytes(*w)).unwrap())
        .collect();
    let stores: Vec<_> = words
        .iter()
        .filter(|i| matches!(i, Instr::Store { .. }))
        .collect();
    assert_eq!(stores.len(), 1, "{words:?}");
    assert!(matches!(
        stores[0],
        Instr::Store {
            op: StoreOp::B,
            offset: 0,
            ..
        }
    ));
    assert_eq!(words.iter().filter(|i| **i == Instr::Ecall).count(), 1);
    assert_eq!(words.last(), Some(&Instr::Ecall));
    assert!(!words.contains(&Instr::Ebreak));
}

// ---------------------------------------------------------------------------------------
// End to end.

#[test]
fn hello_prints_exactly_through_the_cpu_bus_and_uart() {
    let run = fresh(&image(), true);
    let o = &run.finished.outcome;
    println!(
        "{}; gp {:#x}, a0 {:#x}, instret {}, events {}",
        o.end, o.gp, o.a0, o.instret, o.events
    );
    assert_eq!(judge(&run, true), Ok(()));

    // Architectural completion: ECALL with the pass convention, not the limit.
    assert!(matches!(&o.end, End::Trap { cause, .. } if cause == PASS_CAUSE));
    assert_eq!((o.gp, o.a0), (1, 0));
    // 5 set-up instructions, 4 per byte, then gp and a0; the trapping ECALL does not
    // retire.
    assert_eq!(o.instret, 5 + 4 * 20 + 2);

    // Two independent observations of the same bytes.
    let output = run.output.as_ref().unwrap();
    assert_eq!(output.as_slice(), EXPECTED_OUTPUT);
    assert_eq!(output.len(), EXPECTED_OUTPUT.len());
    assert!(!output.contains(&0));
    assert_eq!(run.trace_tx.as_deref(), Some(EXPECTED_OUTPUT.as_slice()));

    // Each byte went through the bus as a one-byte write to the absolute TX address, and
    // reached the UART as offset 0: one request and one `Done` per byte.
    let cpu_writes: Vec<&MemMsg> = run
        .finished
        .dispatched
        .iter()
        .filter(|ev| ev.source == CPU && ev.target == BUS)
        .filter_map(mem)
        .filter(|m| matches!(m, MemMsg::WriteReq { .. }))
        .collect();
    assert_eq!(cpu_writes.len(), 20);
    for (m, &byte) in cpu_writes.iter().zip(EXPECTED_OUTPUT) {
        assert!(
            matches!(m, MemMsg::WriteReq { addr, data, .. }
                if *addr == u64::from(UART_BASE) && data.as_slice() == [byte]),
            "{m:?}"
        );
    }
    let (requests, responses) = uart_traffic(&run.finished.dispatched);
    assert_eq!((requests.len(), responses.len()), (20, 20));
    for (m, &byte) in requests.iter().zip(EXPECTED_OUTPUT) {
        assert!(
            matches!(m, MemMsg::WriteReq { addr, data, .. }
                if *addr == uart::TX && data.as_slice() == [byte]),
            "{m:?}"
        );
    }
}

/// A run's result without the event count, which a resumed run counts from its start.
fn result(run: &HelloRun) -> (runner::Outcome, Vec<u8>, Option<Vec<u8>>) {
    let o = runner::Outcome {
        events: 0,
        ..run.finished.outcome.clone()
    };
    (o, run.output.clone().unwrap(), run.trace_tx.clone())
}

/// A small M1-A7 smoke: tracing and an extra observer change nothing. The full O0-O5
/// matrix is M1.8.
#[test]
fn observation_changes_neither_output_nor_digests() {
    struct Count(Rc<Cell<u64>>);
    impl Observer for Count {
        fn on_after_dispatch(&mut self, _: &EventView<'_>, _: &WorldView<'_>) -> Control {
            self.0.set(self.0.get() + 1);
            Control::Continue
        }
    }
    let image = image();
    let plain = fresh(&image, false);
    let traced = fresh(&image, true);
    let count = Rc::new(Cell::new(0));
    let observed = hello::run(
        &image,
        Start::Init { traced: false },
        vec![Box::new(Count(Rc::clone(&count)))],
    );
    let again = fresh(&image, true);
    assert_eq!(result(&traced), result(&again), "reproducible");
    assert_eq!(result(&plain), result(&observed));
    let (mut t, output, _) = result(&traced);
    t.trace = None;
    assert_eq!((t, output, None), result(&plain));
    assert_eq!(count.get(), plain.finished.outcome.events);
    assert_eq!(
        plain.finished.dispatched, traced.finished.dispatched,
        "the same events"
    );
}

// ---------------------------------------------------------------------------------------
// Checkpoints.

/// Stops after `k` events, then continues from the snapshot in a fresh platform.
fn resumed(image: &LoadImage, k: usize) -> HelloRun {
    let mut rt = runner::platform(image, true);
    rt.start_trace().unwrap();
    rt.init().unwrap();
    for _ in 0..k {
        rt.step().unwrap().unwrap();
    }
    let snapshot = rt.snapshot().unwrap();
    let prefix = rt.take_trace();
    hello::run(image, Start::Restore { snapshot, prefix }, Vec::new())
}

/// M1-A6 for `hello.elf`: resuming from every event boundary but the last gives the same
/// output, trace bytes, final state, and `StateDigest`, `ExecutionDigest`, and
/// `TraceDigest` as never stopping, and replays exactly the remaining events: no byte is
/// lost or repeated, no store reissued, and no pending response sent twice.
#[test]
fn every_checkpoint_resumes_to_the_same_output_and_digests() {
    let image = image();
    let reference = fresh(&image, true);
    let expected = result(&reference);
    let events = &reference.finished.dispatched;
    let n = events.len();

    // The boundaries M1-A6 names, located in the reference run.
    let at_uart: Vec<usize> = (0..n).filter(|&i| events[i].target == UART).collect();
    let from_uart: Vec<usize> = (0..n).filter(|&i| events[i].source == UART).collect();
    let named = [
        ("before the first UART write", at_uart[0]),
        ("with the first WriteResp pending", at_uart[0] + 1),
        ("after ten bytes", from_uart[9] + 1),
        ("with the last WriteResp pending", at_uart[19] + 1),
        ("after the last byte, before ECALL", from_uart[19] + 1),
    ];
    for (name, k) in named {
        assert!(k < n, "{name}: {k} of {n}");
        println!("{name}: after {k} of {n} events");
    }

    // After the last event nothing is left to run, so there is no view to read.
    for k in 0..n {
        let run = resumed(&image, k);
        assert_eq!(judge(&run, false), Ok(()), "checkpoint after {k} events");
        assert_eq!(result(&run), expected, "checkpoint after {k} events");
        assert_eq!(
            run.finished.dispatched,
            events[k..],
            "checkpoint after {k} events"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The checks see real execution.

/// Runs `image` with the word at code offset `at` replaced by `patch(word)`.
fn patched(at: usize, patch: impl Fn(u32) -> u32) -> HelloRun {
    let mut image = image();
    let code = image.segments.iter_mut().find(|s| s.offset == 0).unwrap();
    let word = u32::from_le_bytes(code.bytes[at..at + 4].try_into().unwrap());
    code.bytes[at..at + 4].copy_from_slice(&patch(word).to_le_bytes());
    fresh(&image, true)
}

/// The `SB` at code offset 0x18.
const SB_AT: usize = 0x18;

#[test]
fn a_changed_message_byte_changes_the_output() {
    let mut image = image();
    let data = image
        .segments
        .iter_mut()
        .flat_map(|s| {
            s.bytes
                .windows(20)
                .position(|w| w == EXPECTED_OUTPUT)
                .map(|i| (s, i))
        })
        .next()
        .map(|(s, i)| &mut s.bytes[i])
        .expect("the message is in the image");
    *data = b'J';
    let run = fresh(&image, true);
    assert_eq!(
        run.output.as_deref(),
        Ok(b"Jello, SystemScope!\n".as_slice())
    );
    assert_eq!(
        run.trace_tx.as_deref(),
        Some(b"Jello, SystemScope!\n".as_slice())
    );
    assert!(judge(&run, true).unwrap_err().contains("Jello"));
}

#[test]
fn a_store_to_another_uart_offset_faults_and_prints_nothing() {
    assert_eq!(
        patched(SB_AT, |w| w).output.as_deref(),
        Ok(EXPECTED_OUTPUT.as_slice())
    );
    for offset in [1, 4, 8] {
        // SB's imm[4:0] is bits 11:7.
        let run = patched(SB_AT, |w| w | (offset << 7));
        assert!(
            matches!(&run.finished.outcome.end, End::Trap { cause, .. } if cause == "StoreAccessFault"),
            "offset {offset}: {}",
            run.finished.outcome.end
        );
        assert_eq!(run.output.as_deref(), Ok(&[][..]), "offset {offset}");
        assert!(judge(&run, true).is_err());
    }
}

#[test]
fn a_word_or_halfword_store_to_tx_faults() {
    for funct3 in [0b001, 0b010] {
        let run = patched(SB_AT, |w| w | (funct3 << 12));
        assert!(
            matches!(&run.finished.outcome.end, End::Trap { cause, .. } if cause == "StoreAccessFault"),
            "funct3 {funct3}: {}",
            run.finished.outcome.end
        );
        assert_eq!(run.output.as_deref(), Ok(&[][..]));
        assert!(judge(&run, true).is_err());
    }
}
