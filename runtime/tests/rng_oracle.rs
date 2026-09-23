//! Cross-checks the runtime's generator against an independent implementation.
//!
//! `rand_xoshiro` is a dev-only oracle: the runtime never depends on it, so a change in
//! that crate can only break this test, never the simulation.

use proptest::prelude::*;
use rand_core::{Rng, SeedableRng};
use systemscope_contracts::rng::SimRng;
use systemscope_runtime::rng::{SEED_CONTEXT, Xoshiro256StarStar};

fn derive(seed: u64, path: &str) -> [u8; 32] {
    let mut input = seed.to_le_bytes().to_vec();
    input.extend_from_slice(&(path.len() as u32).to_le_bytes());
    input.extend_from_slice(path.as_bytes());
    blake3::derive_key(SEED_CONTEXT, &input)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn component_streams_match_oracle(seed in any::<u64>(), path in "[a-z0-9._]{0,24}") {
        let mut ours = Xoshiro256StarStar::for_component(seed, &path);
        let mut oracle = rand_xoshiro::Xoshiro256StarStar::from_seed(derive(seed, &path));
        for _ in 0..64 {
            prop_assert_eq!(ours.next_u64(), oracle.next_u64());
        }
    }

    #[test]
    fn raw_seeds_match_oracle(seed in any::<[u8; 32]>()) {
        prop_assume!(seed != [0; 32]);
        let mut ours = Xoshiro256StarStar::from_seed_bytes(seed);
        let mut oracle = rand_xoshiro::Xoshiro256StarStar::from_seed(seed);
        for _ in 0..64 {
            prop_assert_eq!(ours.next_u64(), oracle.next_u64());
        }
    }
}
