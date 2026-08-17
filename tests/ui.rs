//! Compile-fail cases: the derive's rejections, with their exact messages
//! locked in so a refactor cannot silently degrade them.
#![cfg(feature = "derive")]

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/*.rs");
}
