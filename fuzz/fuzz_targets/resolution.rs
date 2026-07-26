//! Every reference is either resolved or accounted for, whatever the input says.
//!
//! Resolution is where a string becomes an address, which is where a generator can quietly point
//! at the wrong thing. Two properties hold for any input at all: the accounting adds up — every
//! reference counted is one that resolved, was repaired, dangled, or named another document, with
//! no fifth outcome and nothing lost — and following a chain of references terminates, however the
//! input arranges them.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The loader catches a panic from the C-derived YAML parser and rejects the document; the
    // fuzzer's own hook would abort before that could happen. See `allow_caught_panics`.
    progeny::harness::allow_caught_panics();
    let Ok(counts) = progeny::harness::resolution(data) else {
        // A rejected document is a fine outcome; it is a panic that is not.
        return;
    };
    assert_eq!(
        counts.references,
        counts.resolved + counts.repaired + counts.dangling + counts.external,
        "a reference was counted but not accounted for: {counts:?}"
    );
    assert!(
        counts.dangling_components <= counts.component_references,
        "more component references dangled than were found: {counts:?}"
    );
});
