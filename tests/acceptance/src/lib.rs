//! M0 acceptance harness (`docs/m0-design.md` §9): AT-1, AT-2, and AT-3 on the full
//! `m0-reference`, and the golden files that `cargo xtask bless` writes.
//!
//! Every check returns a `Result` instead of asserting, so the tests can also feed it
//! doctored inputs and prove that it notices them.

pub mod checkpoint;
pub mod digests;
pub mod golden;
pub mod layout;
pub mod observation;
pub mod process;

/// Seeds with golden digests, run on every CI run (§9.1).
pub const FIXED_SEEDS: [u64; 3] = [0, 1, 0xDEAD_BEEF];

/// Environment variable holding the nightly random seed, in decimal or `0x` hex.
pub const SEED_VAR: &str = "M0_SEED";

/// Parses a seed in decimal or `0x` hex.
pub fn parse_seed(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(&hex.replace('_', ""), 16).ok(),
        None => text.replace('_', "").parse().ok(),
    }
}

/// The nightly seed from [`SEED_VAR`]. Printed first, so a failing run names its seed.
///
/// # Panics
///
/// If the variable is unset or not a seed.
pub fn seed_from_env() -> u64 {
    let text = std::env::var(SEED_VAR)
        .unwrap_or_else(|_| panic!("set {SEED_VAR} to the seed to test, e.g. {SEED_VAR}=0x1234"));
    let seed = parse_seed(&text).unwrap_or_else(|| panic!("{SEED_VAR}={text:?} is not a seed"));
    println!("{SEED_VAR}={seed:#x}");
    seed
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The 32 bytes a 64-digit hex string names.
pub fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_parse_in_decimal_and_hex() {
        assert_eq!(parse_seed("0"), Some(0));
        assert_eq!(parse_seed(" 3735928559\n"), Some(0xDEAD_BEEF));
        assert_eq!(parse_seed("0xdead_beef"), Some(0xDEAD_BEEF));
        assert_eq!(parse_seed("0XFFFFFFFFFFFFFFFF"), Some(u64::MAX));
        assert_eq!(parse_seed(""), None);
        assert_eq!(parse_seed("0x"), None);
        assert_eq!(parse_seed("-1"), None);
    }

    #[test]
    fn hex_round_trips() {
        let bytes: [u8; 32] = std::array::from_fn(|i| (i * 37) as u8);
        assert_eq!(unhex32(&hex(&bytes)), Some(bytes));
        assert_eq!(unhex32("00"), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
    }
}
