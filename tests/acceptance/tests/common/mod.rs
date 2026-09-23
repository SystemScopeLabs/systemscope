//! The committed golden files. Tests only read them; `cargo xtask bless` writes them.

use systemscope_acceptance::golden::Golden;

/// `tests/golden/m0-reference.json`.
pub fn golden() -> Golden {
    let golden = Golden::parse(include_str!("../../../golden/m0-reference.json"))
        .unwrap_or_else(|e| panic!("tests/golden/m0-reference.json: {e}"));
    golden.ensure_current().unwrap_or_else(|e| panic!("{e}"));
    golden
}
