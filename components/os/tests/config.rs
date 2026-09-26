//! The kernel configuration and its whitelist (`docs/m3-design.md` §6.2): the validation
//! rules, the granted ranges, and that `kgate` is never permitted whatever the grants.

mod common;

use common::layout::*;
use proptest::prelude::*;
use systemscope_contracts::time::ClockDomainId;
use systemscope_os::config::{FRAME_ACTION, FRAME_BYTES, FRAME_REASON, FRAME_SEPC};
use systemscope_os::{KernelConfig, KernelConfigError, ModeledKernel, Window};

fn config() -> KernelConfig {
    common::layout::config(ClockDomainId(0))
}

fn window(base: u64, size: u64) -> Window {
    Window { base, size }
}

#[test]
fn the_reference_layout_is_valid() {
    config().validate().unwrap();
    assert!(ModeledKernel::new(config()).is_ok());
    assert_eq!(
        (FRAME_BYTES, FRAME_SEPC, FRAME_ACTION, FRAME_REASON),
        (0x98, 0x7C, 0x90, 0x94)
    );
    assert_eq!(
        config().grants(),
        [
            window(u64::from(TRAP_FRAME), 0x98),
            window(STAGING, STAGING_SIZE),
            window(POOL, POOL_SIZE),
            window(BLK_BASE, BLK_SIZE),
            window(UART_BASE, 1),
        ]
    );
}

#[test]
fn validation_rejects_each_broken_rule() {
    let cases: Vec<(KernelConfig, KernelConfigError)> = vec![
        (
            KernelConfig {
                staging: window(STAGING, 0),
                ..config()
            },
            KernelConfigError::Empty("staging"),
        ),
        (
            KernelConfig {
                gate: window(KGATE_BASE, 0),
                ..config()
            },
            KernelConfigError::Empty("kgate"),
        ),
        (
            KernelConfig {
                blk: window(u64::MAX, 2),
                ..config()
            },
            KernelConfigError::Wraps("the block controller"),
        ),
        (
            KernelConfig {
                trap_frame: TRAP_FRAME + 2,
                ..config()
            },
            KernelConfigError::MisalignedFrame(TRAP_FRAME + 2),
        ),
        (
            KernelConfig {
                trap_frame: 0x1000_0000,
                ..config()
            },
            KernelConfigError::OutsideRam("the trap frame"),
        ),
        (
            KernelConfig {
                trap_frame: (RAM_BASE + RAM_SIZE - 0x94) as u32,
                ..config()
            },
            KernelConfigError::OutsideRam("the trap frame"),
        ),
        (
            KernelConfig {
                frame_pool: window(POOL, POOL_SIZE + 1),
                ..config()
            },
            KernelConfigError::OutsideRam("the frame pool"),
        ),
        (
            KernelConfig {
                trap_frame: STAGING as u32 + 0x100,
                ..config()
            },
            KernelConfigError::Overlap("the trap frame", "staging"),
        ),
        (
            KernelConfig {
                frame_pool: window(STAGING + STAGING_SIZE - 1, 0x1000),
                ..config()
            },
            KernelConfigError::Overlap("staging", "the frame pool"),
        ),
    ];
    for (bad, err) in cases {
        assert_eq!(bad.validate(), Err(err), "{err}");
        assert_eq!(ModeledKernel::new(bad).err(), Some(err));
    }
}

#[test]
fn the_whitelist_grants_exactly_the_ranges() {
    let c = config();
    assert!(c.permits(u64::from(TRAP_FRAME), 16));
    assert!(c.permits(u64::from(TRAP_FRAME) + 0x90, 8));
    assert!(
        !c.permits(u64::from(TRAP_FRAME) + 0x90, 9),
        "past the frame"
    );
    assert!(!c.permits(u64::from(TRAP_FRAME) - 1, 2));
    assert!(c.permits(STAGING + STAGING_SIZE - 16, 16));
    assert!(!c.permits(STAGING + STAGING_SIZE - 15, 16));
    assert!(c.permits(POOL, 16));
    assert!(c.permits(BLK_BASE + 0x1C, 4));
    assert!(c.permits(UART_BASE, 1));
    assert!(!c.permits(UART_BASE, 2));
    assert!(!c.permits(UART_BASE + 1, 1), "only TX");
    // Firmware, the rest of RAM, and kgate are not granted.
    assert!(!c.permits(RAM_BASE, 4));
    assert!(!c.permits(KGATE_BASE, 4));
    assert!(!c.permits(IRQC_BASE, 4));
}

#[test]
fn kgate_is_refused_even_inside_a_grant() {
    // A platform built wrong: the UART grant and a block-controller window over kgate.
    for c in [
        KernelConfig {
            uart_tx: KGATE_BASE,
            ..config()
        },
        KernelConfig {
            blk: window(BLK_BASE, 0x2000),
            ..config()
        },
    ] {
        c.validate().unwrap();
        for off in 0..KGATE_SIZE {
            assert!(!c.permits(KGATE_BASE + off, 1), "{off}");
        }
        assert!(!c.permits(KGATE_BASE - 1, 2));
    }
}

#[test]
fn windows_contain_and_overlap_without_wrapping() {
    let w = window(0x100, 0x10);
    assert!(w.contains(0x100, 0x10));
    assert!(!w.contains(0x100, 0x11));
    assert!(!w.contains(0xFF, 1));
    assert!(!w.contains(0x100, 0), "an empty range is not contained");
    assert!(w.overlaps(0x10F, 1));
    assert!(!w.overlaps(0x110, 1));
    assert!(!w.overlaps(0xF0, 0x10));
    assert!(w.overlaps(0xF0, 0x11));
    assert!(!w.overlaps(0x100, 0));
    assert!(!w.contains(u64::MAX, 2));
    assert!(!w.overlaps(u64::MAX, 2));
    let top = window(u64::MAX - 1, 1);
    assert!(top.contains(u64::MAX - 1, 1));
}

proptest! {
    /// Whatever the grants, an access that touches the gate window is never permitted,
    /// and a permitted access lies inside one grant.
    #[test]
    fn kgate_is_never_permitted(
        gate_base in 0u64..0x2000,
        gate_size in 1u64..0x40,
        uart in 0u64..0x2000,
        blk_base in 0u64..0x2000,
        blk_size in 1u64..0x400,
        addr in 0u64..0x2400,
        len in 1u64..0x40,
    ) {
        let c = KernelConfig {
            gate: window(gate_base, gate_size),
            uart_tx: uart,
            blk: window(blk_base, blk_size),
            ..config()
        };
        let touches = addr < gate_base + gate_size && gate_base < addr + len;
        if touches {
            prop_assert!(!c.permits(addr, len));
        }
        if c.permits(addr, len) {
            prop_assert!(c.grants().iter().any(|g| g.contains(addr, len)));
        }
    }
}
