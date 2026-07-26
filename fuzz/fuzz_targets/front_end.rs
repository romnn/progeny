//! The generator must not panic on any input.
//!
//! A panic is a rejection without a diagnostic — the forbidden failure mode wearing a stack
//! trace — so "no input panics" is a property rather than a hope. This target feeds arbitrary
//! bytes through load, normalization, parsing and serialization, and cares only that the process
//! survives: a rejection is a perfectly good answer.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = progeny::harness::front_end(data);
});
