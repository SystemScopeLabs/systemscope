//! Per-component random streams (`docs/m0-design.md` §5.2).
//!
//! The generator and seed derivation are written out here rather than taken from a
//! library, so their output cannot change with a dependency upgrade.

use systemscope_contracts::rng::SimRng;

/// BLAKE3 `derive_key` context for seeding. Part of the contract: changing it changes
/// every stream. Follows BLAKE3's recommended `[application] [date] [purpose]` form.
pub const SEED_CONTEXT: &str = "SystemScope 2026-09 SimRng v1";

/// The exact bytes fed to `derive_key`: `session_seed` as `u64` LE, then
/// `component_path` as a `u32` LE byte length followed by its UTF-8 bytes.
pub fn seed_material(session_seed: u64, component_path: &str) -> Vec<u8> {
    let path = component_path.as_bytes();
    let len = u32::try_from(path.len()).expect("component path shorter than 4 GiB");
    let mut input = Vec::with_capacity(12 + path.len());
    input.extend_from_slice(&session_seed.to_le_bytes());
    input.extend_from_slice(&len.to_le_bytes());
    input.extend_from_slice(path);
    input
}

/// xoshiro256** (Blackman and Vigna), with a 256-bit state that is never all zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xoshiro256StarStar {
    s: [u64; 4],
}

impl Xoshiro256StarStar {
    /// Seeds a component's stream from the session seed and its path.
    ///
    /// Input to `derive_key` is `session_seed` as `u64` LE followed by `component_path`
    /// as a `u32` LE byte length and its UTF-8 bytes (canonical encoding, §4.5).
    pub fn for_component(session_seed: u64, component_path: &str) -> Xoshiro256StarStar {
        let material = seed_material(session_seed, component_path);
        Xoshiro256StarStar::from_seed_bytes(blake3::derive_key(SEED_CONTEXT, &material))
    }

    /// Builds a generator from 32 seed bytes read as four little-endian `u64`s.
    ///
    /// An all-zero seed, which xoshiro cannot use, becomes the state `[1, 0, 0, 0]`.
    pub fn from_seed_bytes(seed: [u8; 32]) -> Xoshiro256StarStar {
        let mut s = [0u64; 4];
        let (words, _) = seed.as_chunks::<8>();
        for (word, bytes) in s.iter_mut().zip(words) {
            *word = u64::from_le_bytes(*bytes);
        }
        if s == [0; 4] {
            s = [1, 0, 0, 0];
        }
        Xoshiro256StarStar { s }
    }

    /// The full generator state, for snapshots.
    pub fn state(&self) -> [u64; 4] {
        self.s
    }

    /// Restores a generator from a snapshot state. Rejects the invalid all-zero state.
    pub fn from_state(s: [u64; 4]) -> Option<Xoshiro256StarStar> {
        (s != [0; 4]).then_some(Xoshiro256StarStar { s })
    }
}

impl SimRng for Xoshiro256StarStar {
    fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_c_implementation() {
        // Seed words 1, 2, 3, 4; values produced by the reference implementation and
        // published with the rand_xoshiro crate.
        let mut seed = [0u8; 32];
        let (words, _) = seed.as_chunks_mut::<8>();
        for (i, word) in words.iter_mut().enumerate() {
            *word = (i as u64 + 1).to_le_bytes();
        }
        let mut rng = Xoshiro256StarStar::from_seed_bytes(seed);
        let expected: [u64; 10] = [
            11520,
            0,
            1509978240,
            1215971899390074240,
            1216172134540287360,
            607988272756665600,
            16172922978634559625,
            8476171486693032832,
            10595114339597558777,
            2904607092377533576,
        ];
        for e in expected {
            assert_eq!(rng.next_u64(), e);
        }
    }

    #[test]
    fn all_zero_seed_uses_documented_fallback() {
        let rng = Xoshiro256StarStar::from_seed_bytes([0; 32]);
        assert_eq!(rng.state(), [1, 0, 0, 0]);
        assert_eq!(Xoshiro256StarStar::from_state([0; 4]), None);
    }

    #[test]
    fn state_round_trip_resumes_the_exact_stream() {
        let mut a = Xoshiro256StarStar::for_component(7, "soc.cpu0");
        for _ in 0..1000 {
            a.next_u64();
        }
        let mut b = Xoshiro256StarStar::from_state(a.state()).unwrap();
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn seed_context_and_material_are_pinned() {
        assert_eq!(SEED_CONTEXT, "SystemScope 2026-09 SimRng v1");
        let material = seed_material(0x0102_0304_0506_0708, "soc.cpu0");
        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // session_seed, u64 LE
            0x08, 0x00, 0x00, 0x00,                         // path length, u32 LE
            b's', b'o', b'c', b'.', b'c', b'p', b'u', b'0', // path, UTF-8
        ];
        assert_eq!(material, expected);
        let key = blake3::derive_key(SEED_CONTEXT, &material);
        assert_eq!(key, GOLDEN_KEY);
    }

    const GOLDEN_KEY: [u8; 32] = [
        0x02, 0x14, 0x57, 0x28, 0x3a, 0x60, 0x31, 0x89, 0xef, 0xf9, 0x73, 0xcf, 0x59, 0xa2, 0x02,
        0x62, 0xf3, 0xf3, 0x07, 0xf1, 0x14, 0x6d, 0xa9, 0x6f, 0xf4, 0x79, 0x2e, 0x3a, 0xc3, 0xc4,
        0xd8, 0x5a,
    ];

    #[test]
    fn seed_derivation_is_pinned() {
        // Golden values: any change to the context string, input encoding, or generator
        // must fail here and requires a deliberate re-bless.
        let mut rng = Xoshiro256StarStar::for_component(0, "soc.cpu0");
        let first: Vec<u64> = (0..3).map(|_| rng.next_u64()).collect();
        assert_eq!(first, GOLDEN_SOC_CPU0_SEED0);
    }

    const GOLDEN_SOC_CPU0_SEED0: [u64; 3] = [
        5712282200546729087,
        2681783187644526198,
        8218977661140234291,
    ];

    #[test]
    fn streams_depend_on_seed_and_path() {
        let draw = |seed, path| Xoshiro256StarStar::for_component(seed, path).next_u64();
        assert_ne!(draw(0, "soc.cpu0"), draw(1, "soc.cpu0"));
        assert_ne!(draw(0, "soc.cpu0"), draw(0, "soc.cpu1"));
        // Length prefix: "ab" + seed must not collide with "a" + shifted bytes.
        assert_ne!(draw(0, "ab"), draw(0, "a"));
        assert_eq!(draw(42, "x"), draw(42, "x"));
    }
}
