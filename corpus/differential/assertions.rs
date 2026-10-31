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
fn preserved_presence_distinguishes_omit_null_and_value_on_the_wire() {
    let (omitted_left, omitted_right) = both(r#"{"required":"a"}"#)?
        .map_err(|(left, right)| eyre::eyre!("both refused omission: {left} / {right}"))?;
    assert_eq!(omitted_left, omitted_right);
    assert!(!omitted_left.contains("\"clearable\""), "{omitted_left}");

    let (null_left, null_right) = both(r#"{"required":"a","clearable":null}"#)?
        .map_err(|(left, right)| eyre::eyre!("both refused null: {left} / {right}"))?;
    assert_eq!(null_left, null_right);
    assert!(null_left.contains("\"clearable\":null"), "{null_left}");

    let (value_left, value_right) = both(r#"{"required":"a","clearable":"set"}"#)?
        .map_err(|(left, right)| eyre::eyre!("both refused a value: {left} / {right}"))?;
    assert_eq!(value_left, value_right);
    assert!(
        value_left.contains("\"clearable\":\"set\""),
        "{value_left}"
    );
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
    // A string the enum does not list is *accepted* by both — every enum is open — so the wrong
    // type for it is a number.
    refuse_modulo_offset(r#"{"required":"a","state":5}"#)?;
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
    for payload in [r#""in-progress""#, r#""done""#, r#""unknown""#] {
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

/// Every string enum is open: a value the description does not list lands in the fallback
/// variant, keeps its bytes, and writes back exactly as it arrived — under both strategies,
/// because the string enum takes the one hand-written path under both.
#[test_util::test]
fn an_unknown_variant_is_kept_the_same_way() {
    let payload = r#""nonsense""#;
    let left = serde_json::from_str::<derived::types::State>(payload)?;
    let right = serde_json::from_str::<hand::types::State>(payload)?;
    assert!(matches!(left, derived::types::State::Unknown(ref raw) if raw == "nonsense"));
    assert!(matches!(right, hand::types::State::Unknown(ref raw) if raw == "nonsense"));
    assert_eq!(left.as_str(), "nonsense");
    assert_eq!(serde_json::to_string(&left)?, payload);
    assert_eq!(serde_json::to_string(&right)?, payload);
}

/// A declared `unknown` is a declared value, not the fallback, under both strategies.
#[test_util::test]
fn a_declared_unknown_value_is_not_the_fallback() {
    let payload = r#""unknown""#;
    let left = serde_json::from_str::<derived::types::State>(payload)?;
    let right = serde_json::from_str::<hand::types::State>(payload)?;
    assert!(matches!(left, derived::types::State::Unknown_));
    assert!(matches!(right, hand::types::State::Unknown_));
}

/// The read form under both strategies: the same value out of a drifted payload, the same
/// report, and the same bytes written back.
#[test_util::test]
fn the_read_form_decodes_and_reports_the_same_way() {
    let payload = r#"{"optional":"seven","clearable":null,"state":"paused","undeclared":[1]}"#;
    let left = derived::types::Decoded::<derived::types::read::Spike>::from_json(payload)?;
    let right = hand::types::Decoded::<hand::types::read::Spike>::from_json(payload)?;
    assert_eq!(
        left.degradations.to_string(),
        right.degradations.to_string()
    );
    // Decoded on its own, a read form keys its report at its own site under both strategies.
    assert_eq!(
        left.degradations.root(),
        derived::types::read::Spike::SITE
    );
    assert_eq!(right.degradations.root(), hand::types::read::Spike::SITE);
    // A required member absent, a member of the wrong type, an unlisted enum value, and an
    // undeclared member: four sites, four entries.
    assert_eq!(left.degradations.len(), 4, "{}", left.degradations);
    assert_eq!(left.value.required, None);
    assert_eq!(left.value.optional, None);
    assert_eq!(right.value.optional, None);
    assert!(matches!(
        left.value.state,
        Some(derived::types::State::Unknown(ref raw)) if raw == "paused"
    ));
    assert_eq!(
        serde_json::to_string(&left.value)?,
        serde_json::to_string(&right.value)?
    );
    assert_eq!(
        serde_json::to_string(&right.value)?,
        r#"{"clearable":null,"state":"paused","undeclared":[1]}"#
    );
}

/// Payloads generated from the contract itself: every subset of the optional members, with each
/// member present, absent and null.
///
/// The contract knows every member, its presence and its type, so the generator falls out of it —
/// which is the cheap version of the property-based layer, and it is what turns "the cases I thought
/// of" into "every combination of them".
#[test_util::test]
fn every_combination_of_presence_agrees() {
    const MEMBERS: [(&str, [&str; 2]); 5] = [
        ("optional", ["1", "null"]),
        ("clearable", [r#""set""#, "null"]),
        ("wireName", ["true", "null"]),
        ("limit", ["7", "null"]),
        ("state", [r#""done""#, "null"]),
    ];
    // Three states per member — present, present-and-null, absent — over five members.
    for mask in 0..3_u32.pow(5) {
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

/// Read a tool through both renderings and write it back, refusing a payload either refuses.
fn tool(payload: &str) -> eyre::Result<(derived::types::Tool, String, String)> {
    let left = serde_json::from_str::<derived::types::Tool>(payload)?;
    let right = serde_json::from_str::<hand::types::Tool>(payload)?;
    let written = serde_json::to_string(&left)?;
    assert_eq!(written, serde_json::to_string(&right)?, "{payload}");
    Ok((left, written, payload.to_owned()))
}

/// A union whose branches name themselves picks the branch every spelling, a missing tag and a
/// `null` one name, and writes the tag back the way the payload had it.
#[test_util::test]
fn a_self_naming_union_reads_every_way_a_branch_is_named_and_keeps_it() {
    use derived::types::Tool;

    // A missing tag is the custom tool, and it stays missing.
    let (read, written, payload) = tool(r#"{"name":"mine"}"#)?;
    assert!(matches!(read, Tool::Custom(_)), "{read:?}");
    assert_eq!(written, payload);
    // A `null` tag is the nullish tool, and it stays `null`.
    let (read, written, payload) = tool(r#"{"name":"mine","type":null}"#)?;
    assert!(matches!(read, Tool::Nullish(_)), "{read:?}");
    assert_eq!(written, payload);
    // Both spellings of the search tool read as it, and each is written back as it was.
    for spelling in ["search_v2", "search"] {
        let (read, written, payload) = tool(&format!(r#"{{"name":"web","type":"{spelling}"}}"#))?;
        assert!(matches!(read, Tool::SearchV2(_)), "{read:?}");
        assert_eq!(written, payload);
    }
    // A tool named once is read and written as before.
    let (read, written, _) = tool(r#"{"name":"sh","type":"bash"}"#)?;
    assert!(matches!(read, Tool::Bash(_)), "{read:?}");
    assert!(written.contains(r#""type":"bash""#), "{written}");
    // A tag of another kind, or one no branch lists, is kept whole in the open arm.
    for payload in [r#"{"name":"x","type":7}"#, r#"{"name":"x","type":"other"}"#] {
        let (read, written, payload) = tool(payload)?;
        assert!(matches!(read, Tool::Unknown(_)), "{read:?}");
        assert_eq!(written, payload);
    }
}

/// A hand-built value whose own tag names another branch is written with its branch's name, so
/// it reads back as the branch it was built as.
#[test_util::test]
fn a_self_naming_union_writes_the_branch_a_value_was_built_as() {
    let search = derived::types::SearchTool {
        name: "web".to_owned(),
        type_: derived::types::SearchToolType::Unknown("bash".to_owned()),
    };
    let written = serde_json::to_string(&derived::types::Tool::SearchV2(Box::new(search)))?;
    assert!(written.contains(r#""type":"search_v2""#), "{written}");
    let custom = hand::types::CustomTool {
        name: "mine".to_owned(),
        type_: Some(hand::types::CustomToolType::Unknown("bash".to_owned())),
    };
    let written = serde_json::to_string(&hand::types::Tool::Custom(Box::new(custom)))?;
    assert!(written.contains(r#""type":"custom""#), "{written}");
}

/// The read form routes and writes the same way under both strategies.
#[test_util::test]
fn a_self_naming_union_read_form_routes_the_same_way() {
    for payload in [
        r#"{"name":"mine"}"#,
        r#"{"name":"mine","type":null}"#,
        r#"{"name":"web","type":"search"}"#,
    ] {
        let left = derived::types::Decoded::<derived::types::read::Tool>::from_json(payload)?;
        let right = hand::types::Decoded::<hand::types::read::Tool>::from_json(payload)?;
        assert!(left.degradations.is_empty(), "{payload}: {}", left.degradations);
        assert_eq!(serde_json::to_string(&left.value)?, payload);
        assert_eq!(serde_json::to_string(&right.value)?, payload);
    }
}

/// A member and an enum value named by edition 2024's reserved `gen` compile and round-trip.
#[test_util::test]
fn a_name_edition_2024_reserves_is_escaped_and_round_trips() {
    let payload = r#"{"gen":"x","kind":"gen"}"#;
    let left = serde_json::from_str::<derived::types::Keywords>(payload)?;
    let right = serde_json::from_str::<hand::types::Keywords>(payload)?;
    assert_eq!(serde_json::to_string(&left)?, payload);
    assert_eq!(serde_json::to_string(&right)?, payload);
}

/// Read `payload` as `T` and `U`, the two renderings of one type, and write both back.
fn round_trip<T, U>(payload: &str) -> eyre::Result<(String, String)>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
    U: serde::de::DeserializeOwned + serde::Serialize,
{
    let left = serde_json::to_string(&serde_json::from_str::<T>(payload)?)?;
    let right = serde_json::to_string(&serde_json::from_str::<U>(payload)?)?;
    Ok((left, right))
}

/// Members beside a union read into the struct and the union beside it, and are written back
/// among each other, the same bytes under both strategies.
#[test_util::test]
fn members_beside_a_union_round_trip_through_the_flattened_union() {
    for payload in [
        r#"{"type":"image","url":"https://x","detail":"high"}"#,
        r#"{"fileId":"f1","type":"image"}"#,
    ] {
        let (left, right) = round_trip::<
            derived::types::ImageUserInput,
            hand::types::ImageUserInput,
        >(payload)?;
        assert_eq!(left, right, "{payload}");
        // The same members come back, in the order the struct writes them.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&left)?,
            serde_json::from_str::<serde_json::Value>(payload)?,
            "{payload}"
        );
    }
    // The union holds what the struct's own members left.
    let image = serde_json::from_str::<hand::types::ImageUserInput>(
        r#"{"type":"image","url":"u","detail":"high"}"#,
    )?;
    assert_eq!(image.detail.as_deref(), Some("high"));
    assert!(matches!(
        image.variant,
        hand::types::ImageUserInputVariant::Url(ref url) if url.as_str() == "u"
    ));

    // Closed branches decided by shape see only the members the struct leaves them.
    for payload in [
        r#"{"label":"home","lat":1.5,"lon":2.5}"#,
        r#"{"street":"Main","city":"X"}"#,
    ] {
        let (left, right) =
            round_trip::<derived::types::Located, hand::types::Located>(payload)?;
        assert_eq!(left, right, "{payload}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&left)?,
            serde_json::from_str::<serde_json::Value>(payload)?,
            "{payload}"
        );
    }
    let located = serde_json::from_str::<hand::types::Located>(r#"{"street":"Main","city":"X"}"#)?;
    assert_eq!(located.label, None);
    assert!(!matches!(
        located.variant,
        hand::types::LocatedVariant::Unknown(_)
    ));
}

/// A union whose branches name themselves reads each payload by its tag, the branch with a union
/// flattened beside its members included, and writes the same members back under both
/// strategies.
#[test_util::test]
fn a_self_naming_union_reads_a_branch_with_a_flattened_union_by_its_tag() {
    for payload in [
        r#"{"type":"text","text":"hi"}"#,
        r#"{"type":"image","uri":"https://x","detail":"high"}"#,
        r#"{"type":"image","blob":"f1"}"#,
        r#"{"type":"localAudio","path":"/a.wav"}"#,
    ] {
        let (left, right) =
            round_trip::<derived::types::UserInput, hand::types::UserInput>(payload)?;
        assert_eq!(left, right, "{payload}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&left)?,
            serde_json::from_str::<serde_json::Value>(payload)?,
            "{payload}"
        );
    }
    // The tag decides the branch, so a payload no earlier branch could hold is never mistaken
    // for the image.
    let audio = serde_json::from_str::<hand::types::UserInput>(
        r#"{"type":"localAudio","path":"/a.wav"}"#,
    )?;
    assert!(matches!(audio, hand::types::UserInput::LocalAudio(_)));
    let image = serde_json::from_str::<hand::types::UserInput>(
        r#"{"type":"image","uri":"u","detail":"high"}"#,
    )?;
    let hand::types::UserInput::Image(image) = image else {
        bail!("the image payload is not the image variant");
    };
    assert_eq!(image.detail.as_deref(), Some("high"));
}

/// A union matched by shape reads each payload into the branch it fits, whether the branches
/// differ one level down or a closed earlier branch reads what it accepts whole, the same way
/// under both strategies.
#[test_util::test]
fn a_union_matched_by_shape_reads_each_payload_into_its_branch() {
    for payload in [r#"{"items":{"enum":["a"]}}"#, r#"{"items":{"anyOf":["a"]}}"#] {
        let (left, right) =
            round_trip::<derived::types::MultiSelect, hand::types::MultiSelect>(payload)?;
        assert_eq!((left.as_str(), right.as_str()), (payload, payload));
    }
    let titled = serde_json::from_str::<hand::types::MultiSelect>(r#"{"items":{"anyOf":["a"]}}"#)?;
    assert!(matches!(titled, hand::types::MultiSelect::TitledMultiSelect(_)));

    for payload in [r#"{"enum":["a"]}"#, r#"{"enum":["a"],"enumNames":["A"]}"#] {
        let (left, right) =
            round_trip::<derived::types::EnumSchema, hand::types::EnumSchema>(payload)?;
        assert_eq!((left.as_str(), right.as_str()), (payload, payload));
    }
    // The member the earlier branch does not declare makes it refuse, so the later one reads it.
    let legacy =
        serde_json::from_str::<hand::types::EnumSchema>(r#"{"enum":["a"],"enumNames":["A"]}"#)?;
    assert!(matches!(legacy, hand::types::EnumSchema::LegacyEnum(_)));
    let untitled = serde_json::from_str::<hand::types::EnumSchema>(r#"{"enum":["a"]}"#)?;
    assert!(matches!(untitled, hand::types::EnumSchema::UntitledEnum(_)));
}

/// A struct reads an object and nothing else, so a list reaches the list branch after it, and
/// both renderings write each payload back as it came.
#[test_util::test]
fn a_list_reaches_the_list_branch_after_an_object_one() {
    for payload in [r#"["Ada"]"#, r#"{"name":"Ada"}"#] {
        let derived = serde_json::from_str::<derived::types::Either>(payload)?;
        let hand = serde_json::from_str::<hand::types::Either>(payload)?;
        assert_eq!(serde_json::to_string(&derived)?, payload);
        assert_eq!(serde_json::to_string(&hand)?, payload);
    }
}

/// A union of itself and `null` reads `null` and refuses every other value, the same way under
/// both strategies, strictly and through the read form of the struct holding it.
/// A decoder handing a value back to the type it came from would recurse until the stack
/// overflowed, which aborts this whole test binary rather than failing one test.
#[test_util::test]
fn a_union_of_itself_and_null_refuses_other_values_instead_of_recursing() {
    // Strict decoding
    serde_json::from_str::<derived::types::Maybe>("null")?;
    serde_json::from_str::<hand::types::Maybe>("null")?;
    for payload in ["1", "{}", "[null]"] {
        assert!(serde_json::from_str::<derived::types::Maybe>(payload).is_err(), "{payload}");
        assert!(serde_json::from_str::<hand::types::Maybe>(payload).is_err(), "{payload}");
    }
    for payload in [
        r#"{"a":1,"b":null,"c":null,"label":"x"}"#,
        r#"{"a":null,"b":{"a":1},"c":null,"label":"x"}"#,
    ] {
        assert!(
            serde_json::from_str::<derived::types::MaybeHolder>(payload).is_err(),
            "{payload}"
        );
        assert!(
            serde_json::from_str::<hand::types::MaybeHolder>(payload).is_err(),
            "{payload}"
        );
    }

    // Lenient decoding: the type alone refuses, and the read form tolerates the member as absent
    assert!(derived::types::Decoded::<derived::types::Maybe>::from_json("1").is_err());
    assert!(hand::types::Decoded::<hand::types::Maybe>::from_json("1").is_err());
    let payload = r#"{"a":1,"b":null,"c":null,"label":"x"}"#;
    let left = derived::types::Decoded::<derived::types::read::MaybeHolder>::from_json(payload)?;
    let right = hand::types::Decoded::<hand::types::read::MaybeHolder>::from_json(payload)?;
    assert!(left.value.a.is_none() && right.value.a.is_none());
    assert!(!left.degradations.is_empty(), "{}", left.degradations);
    assert!(!right.degradations.is_empty(), "{}", right.degradations);
}

/// A required member typed by the union of itself and `null`, directly or through an alias,
/// holds `null` as a value: both renderings write it back strictly, and the read form, which
/// the required string gives the holder, reads it without reporting it and writes it back too.
/// So does one typed by an alias of a nullable list of itself, which is a newtype once its
/// recursion is promoted, and which holds its lists through every rendering as well.
#[test_util::test]
fn a_null_in_a_required_member_typed_through_an_alias_round_trips() {
    for payload in [
        r#"{"a":null,"b":null,"c":null,"label":"x"}"#,
        r#"{"a":null,"b":null,"c":[null,[[]]],"label":"x"}"#,
    ] {
        let (left, right) =
            round_trip::<derived::types::MaybeHolder, hand::types::MaybeHolder>(payload)?;
        assert_eq!((left.as_str(), right.as_str()), (payload, payload));

        let left =
            derived::types::Decoded::<derived::types::read::MaybeHolder>::from_json(payload)?;
        let right = hand::types::Decoded::<hand::types::read::MaybeHolder>::from_json(payload)?;
        assert!(left.degradations.is_empty(), "{}", left.degradations);
        assert!(right.degradations.is_empty(), "{}", right.degradations);
        assert_eq!(serde_json::to_string(&left.value)?, payload);
        assert_eq!(serde_json::to_string(&right.value)?, payload);
    }
    // The holder is twinned, so its read form is a type of its own rather than a re-export.
    let _: derived::types::read::MaybeHolder = derived::types::MaybeHolder {
        a: None,
        b: None,
        c: None,
        label: "x".to_owned(),
    }
    .into();
}
