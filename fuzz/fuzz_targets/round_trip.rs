//! Whatever the front end accepts, the model holds exactly.
//!
//! Stronger than "does not panic": for every input the front end accepts, the model has to
//! serialize back to the value it was given. A difference is a hole in the model, which is the
//! one defect this project cannot tolerate quietly.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(result) = progeny::harness::round_trip(data) {
        assert!(
            result.is_clean(),
            "the model did not hold the document: {:#?}",
            result.differences
        );
    }
});
