//! Whatever a document says, what comes out is valid Rust — and the same Rust twice.
//!
//! Two properties, both of them things a caller depends on and neither of them checkable from a
//! fixture set. **Every rendered file parses**, because a document that makes progeny emit
//! syntactically invalid source is a bug the consumer's compiler reports rather than progeny;
//! rendering falls back to unformatted tokens when formatting fails, which would otherwise let such
//! a case through quietly. And **generation is deterministic**, because checked-in generated output
//! is only reviewable if the same input produces the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The loader catches a panic from the C-derived YAML parser and rejects the document; the
    // fuzzer's own hook would abort before that could happen. See `allow_caught_panics`.
    progeny::harness::allow_caught_panics();
    let config = progeny::Config::default();
    let Ok(output) = progeny::generate(data, &config) else {
        // A rejected document is a fine outcome; it is a panic that is not.
        return;
    };
    for (path, contents) in &output.files {
        if path.extension() != Some("rs") {
            continue;
        }
        assert!(
            syn::parse_file(contents).is_ok(),
            "{}",
            indoc::formatdoc! {"
                {path} is not valid Rust:
                {contents}"
            }
        );
    }
    let Ok(again) = progeny::generate(data, &config) else {
        panic!("the same input was accepted once already");
    };
    assert_eq!(
        output.files, again.files,
        "generating twice produced different source"
    );
});
