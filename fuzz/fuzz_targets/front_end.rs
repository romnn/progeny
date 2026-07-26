//! The generator must not panic on any input.
//!
//! A panic is a rejection without a diagnostic — the forbidden failure mode wearing a stack
//! trace — so "no input panics" is a property rather than a hope. This target feeds arbitrary
//! bytes through load, normalization, parsing and serialization, and cares only that the process
//! survives: a rejection is a perfectly good answer.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The loader catches a panic from the C-derived YAML parser and rejects the document; the
    // fuzzer's own hook would abort before that could happen. See `allow_caught_panics`.
    progeny::harness::allow_caught_panics();
    let _ = progeny::harness::front_end(data);
});
