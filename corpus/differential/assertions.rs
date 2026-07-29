//! The differential harness: both renderings of one contract, asserted equivalent.
//!
//! `derived` and `hand` are the *same* document generated twice, once with `serde-impl =
//! "derive-always"` and once with `"hand-written-where-eligible"`. Every assertion below is a
//! property the hand-written path has to share with the derive, and each one is a behaviour the
//! predecessor got wrong at least once.
//!
//! Comparison is on the wire, not on the values: the two modules define different Rust types, so
//! "equivalent" means "the same bytes come out and the same bytes go in", which is the only
//! definition that matters to a caller.

use color_eyre::eyre::{self, OptionExt as _, bail};
use differential::{derived, hand};

/// Deserialize a payload through both renderings and compare what came out.
fn both(
    payload: &str,
) -> eyre::Result<Result<(String, String), (String, String)>> {
    let left = serde_json::from_str::<derived::types::Spike>(payload)
        .and_then(|value| serde_json::to_string(&value));
    let right = serde_json::from_str::<hand::types::Spike>(payload)
        .and_then(|value| serde_json::to_string(&value));
    Ok(match (left, right) {
        (Ok(left), Ok(right)) => Ok((left, right)),
        (Err(left), Err(right)) => Err((left.to_string(), right.to_string())),
        (Ok(left), Err(right)) => bail!(
            "the derive accepted {payload} and produced {left}; the hand-written impl refused it: \
             {right}"
        ),
        (Err(left), Ok(right)) => bail!(
            "the derive refused {payload} with {left}; the hand-written impl accepted it and \
             produced {right}"
        ),
    })
}

/// Assert that both renderings accept a payload and write the same bytes back.
fn agree(payload: &str) -> eyre::Result<()> {
    match both(payload)? {
        Ok((left, right)) => assert_eq!(left, right, "re-serializing {payload} disagreed"),
        Err((left, right)) => bail!("both refused {payload}: {left} / {right}"),
    }
    Ok(())
}

/// Assert that both renderings refuse a payload with the same message, byte for byte.
fn refuse(payload: &str) -> eyre::Result<()> {
    match both(payload)? {
        Ok((left, _)) => bail!("both accepted {payload} and produced {left}"),
        Err((left, right)) => assert_eq!(left, right, "the messages for {payload} disagreed"),
    }
    Ok(())
}

/// Assert that both renderings refuse a payload with the same message, and that both say *where* —
/// but not necessarily at the same offset.
///
/// **The one reviewed exception to byte-identical errors.** An error raised while replaying a
/// buffered member cannot name that member's offset in the input, because the format has already
/// read past it: the derive reports the offset of the offending value, the buffered implementation
/// reports the end of the object that held it. The sentence is identical, the offset is not, and no
/// amount of care in the implementation can recover it — buffering is what loses it. serde's own
/// internally-tagged and untagged deserialization has the same property for the same reason.
fn refuse_modulo_offset(payload: &str) -> eyre::Result<()> {
    match both(payload)? {
        Ok((left, _)) => bail!("both accepted {payload} and produced {left}"),
        Err((left, right)) => {
            assert_eq!(
                message(&left),
                message(&right),
                "the messages for {payload} disagreed"
            );
            // Both still have to say where, or the exception would be hiding a lost position.
            assert!(left.contains(" at line "), "{left}");
            assert!(right.contains(" at line "), "{right}");
        }
    }
    Ok(())
}

/// An error message without its trailing offset.
fn message(error: &str) -> &str {
    match error.find(" at line ") {
        Some(at) => error.split_at(at).0,
        None => error,
    }
}

#[test_util::test]
fn a_complete_payload_round_trips_identically() {
    agree(r#"{"required":"a","optional":1,"wireName":true,"limit":5,"state":"done"}"#)?;
}

#[test_util::test]
fn an_absent_optional_member_is_accepted_by_both() {
    // A bare `Option<T>` accepts a missing key with no `default` attribute. Getting this wrong
    // rejects valid responses, which is the expensive direction.
    agree(r#"{"required":"a"}"#)?;
}

#[test_util::test]
fn an_explicit_null_and_an_absent_member_are_treated_alike_by_both() {
    agree(r#"{"required":"a","optional":null}"#)?;
    agree(r#"{"required":"a","wireName":null}"#)?;
}

#[test_util::test]
fn a_declared_default_is_applied_by_neither() {
    let (left, right) = both(r#"{"required":"a"}"#)?
        .map_err(|(left, right)| eyre::eyre!("both refused it: {left} / {right}"))?;
    assert_eq!(left, right);
    // OpenAPI's `default` says what the *server* assumes when a member is absent. Filling it in on
    // the way in means writing it on the way out, which turns "the caller said nothing" into "the
    // caller said 20" — a different request, sent silently. Both renderings decline, and the value
    // is stated in the member's documentation instead.
    assert!(!left.contains("\"limit\""), "{left}");
    assert!(!right.contains("\"limit\""), "{right}");
}

#[test_util::test]
fn a_member_written_under_its_wire_name_is_read_by_both() {
    let (left, right) = both(r#"{"required":"a","wireName":false}"#)?
        .map_err(|(left, right)| eyre::eyre!("both refused it: {left} / {right}"))?;
    assert_eq!(left, right);
    assert!(left.contains("\"wireName\":false"), "{left}");
}

#[test_util::test]
fn an_undeclared_member_is_ignored_by_both() {
    agree(r#"{"required":"a","surprise":{"deep":[1,2]}}"#)?;
}

#[test_util::test]
fn a_missing_required_member_is_refused_the_same_way() {
    refuse(r#"{"optional":1}"#)?;
}

#[test_util::test]
fn a_member_of_the_wrong_type_is_refused_the_same_way() {
    refuse_modulo_offset(r#"{"required":7}"#)?;
    refuse_modulo_offset(r#"{"required":"a","optional":"seven"}"#)?;
    refuse_modulo_offset(r#"{"required":"a","state":"unknown"}"#)?;
}

#[test_util::test]
fn a_payload_that_is_not_an_object_is_refused_the_same_way() {
    refuse("[]")?;
    refuse(r#""a string""#)?;
    refuse("7")?;
}

#[test_util::test]
fn a_duplicate_member_is_refused_the_same_way() {
    // Constructed as raw text on purpose: a `serde_json::Value` cannot hold two members with one
    // name, so a harness that went through `Value` would lose exactly the case being tested.
    refuse(r#"{"required":"a","required":"b"}"#)?;
    refuse(r#"{"required":"a","optional":1,"optional":2}"#)?;
}

#[test_util::test]
fn an_undeclared_member_is_refused_the_same_way_when_the_schema_says_so() {
    let payload = r#"{"required":"a","surprise":1}"#;
    let left = serde_json::from_str::<derived::types::Closed>(payload)
        .err()
        .ok_or_eyre("the derive should refuse it")?
        .to_string();
    let right = serde_json::from_str::<hand::types::Closed>(payload)
        .err()
        .ok_or_eyre("the hand-written impl should refuse it")?
        .to_string();
    assert_eq!(left, right);
}

/// The fixed-arity trailing-element bug class, which only the buffered path can have.
///
/// The tuple's own implementation is derived on both sides — the eligibility function rules tuples
/// to the derive — so what differs is what surrounds it. In derive mode `serde_json` reads the
/// array itself and notices the leftovers. In hand-written mode the struct **buffers** the member
/// and replays it, which makes progeny the format: a replay that stops at the arity it was asked
/// for and never looks again drops the rest in silence. It did exactly that until this test, and
/// the fix is `SeqDeserializer::end()` after `visit_seq`.
#[test_util::test]
fn a_fixed_arity_member_given_too_many_elements_is_refused_by_both() {
    agree(r#"{"required":"a","pair":[1,2]}"#)?;
    both(r#"{"required":"a","pair":[1,2,3]}"#)?
        .err()
        .ok_or_eyre("a third element must not be accepted by either")?;
    both(r#"{"required":"a","pair":[1]}"#)?
        .err()
        .ok_or_eyre("a short pair must not be accepted either")?;
}

/// A struct with no members, which is a shape three corpus documents declare.
///
/// The interesting part is that it *compiles*: every binding in the generated impls has nothing to
/// do, and each one warned — an unused `buffer`, a `mut` on a count nothing increments, a `mut` on
/// a state nothing writes to. Warnings are errors in a consumer's build about code they did not
/// write, so this belongs in the fast gate rather than only in the tier that compiles eight
/// documents.
#[test_util::test]
fn a_struct_with_no_members_round_trips_through_both() {
    let left = serde_json::from_str::<derived::types::Empty>("{}")
        .and_then(|value| serde_json::to_string(&value))
        ?;
    let right = serde_json::from_str::<hand::types::Empty>("{}")
        .and_then(|value| serde_json::to_string(&value))
        ?;
    assert_eq!(left, right);
}

#[test_util::test]
fn a_fieldless_enum_reads_and_writes_the_same_bytes() {
    for payload in [r#""in-progress""#, r#""done""#] {
        let left = serde_json::from_str::<derived::types::State>(payload)
            .and_then(|value| serde_json::to_string(&value))
            ?;
        let right = serde_json::from_str::<hand::types::State>(payload)
            .and_then(|value| serde_json::to_string(&value))
            ?;
        assert_eq!(left, right);
        assert_eq!(left, payload);
    }
}

#[test_util::test]
fn an_unknown_variant_is_refused_the_same_way() {
    let payload = r#""nonsense""#;
    let left = serde_json::from_str::<derived::types::State>(payload)
        .err()
        .ok_or_eyre("the derive should refuse it")?
        .to_string();
    let right = serde_json::from_str::<hand::types::State>(payload)
        .err()
        .ok_or_eyre("the hand-written impl should refuse it")?
        .to_string();
    assert_eq!(left, right);
}

/// Payloads generated from the contract itself: every subset of the optional members, with each
/// member present, absent and null.
///
/// The contract knows every member, its presence and its type, so the generator falls out of it —
/// which is the cheap version of the property-based layer, and it is what turns "the cases I thought
/// of" into "every combination of them".
#[test_util::test]
fn every_combination_of_presence_agrees() {
    const MEMBERS: [(&str, [&str; 2]); 4] = [
        ("optional", ["1", "null"]),
        ("wireName", ["true", "null"]),
        ("limit", ["7", "null"]),
        ("state", [r#""done""#, "null"]),
    ];
    // Three states per member — present, present-and-null, absent — over four members.
    for mask in 0..3_u32.pow(4) {
        let mut members = vec![r#""required":"a""#.to_owned()];
        let mut divisor = 1;
        for (name, values) in MEMBERS {
            let state = (mask / divisor) % 3;
            divisor *= 3;
            match state {
                0 => members.push(format!("\"{name}\":{}", values[0])),
                1 => members.push(format!("\"{name}\":{}", values[1])),
                _ => {}
            }
        }
        let payload = format!("{{{}}}", members.join(","));
        match both(&payload)? {
            Ok((left, right)) => assert_eq!(left, right, "re-serializing {payload} disagreed"),
            Err((left, right)) => assert_eq!(
                message(&left),
                message(&right),
                "the messages for {payload} disagreed"
            ),
        }
    }
}
