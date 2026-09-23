//! AT-3 compile-fail tests (`docs/m0-design.md` §9): observers cannot reach mutable
//! simulation state through `WorldView`, and `inspect` cannot change a component.

#[test]
fn world_view_gives_no_mutable_access() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
