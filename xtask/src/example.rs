//! The example crate: one description, both halves, talking to each other.
//!
//! Every other gate in this project checks one side of the wire. The corpus checks that a document
//! round-trips; the compile gate checks that the emitted source is Rust; the payload gate runs serde
//! against real bodies. **None of them sends a request.** Whether a generated `send()` builds the
//! URL, query string, headers and cookies the description asks for has been checked only by reading
//! the emitted source, which is exactly the kind of claim this project has already been wrong about
//! twice.
//!
//! So this generates the client *and* the server from one description, implements the server's
//! `Api` trait with a double that records what arrived, starts it on a real socket, and calls it
//! with the generated client. A disagreement between the two halves about any part of the request
//! line is a failing test rather than a thing somebody notices in production.
//! A second implementation takes a request context of its own, so the order between extracting it
//! and decoding the request, and the context reaching `on_rejection`, are checked on the wire too.
//! The double also answers with the response headers the subject is given here, so what the
//! server writes for a typed header, its raw override and a reply that fails on the way out are
//! read off the wire as well, and what the client reads back into its typed fields beside them.
//!
//! Its subject is the committed `petstore-31`, which needs no network and whose `rejectionProbe`
//! operation carries a path parameter, a query parameter, a header, a cookie and a body at once.

use std::fmt::Write as _;

use camino::Utf8PathBuf;
use clap::Args as ClapArgs;
use color_eyre::eyre::{self, ContextCompat, WrapErr, bail};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Write the example crate and stop, without compiling or running it.
    #[arg(long)]
    generate_only: bool,

    /// Which serde strategy to generate with, rather than the configuration default.
    #[arg(long, value_name = "STRATEGY")]
    serde: Option<crate::corpus::SerdeChoice>,
}

/// The document the example is built from: committed, tiny, and exercising all four locations.
const SUBJECT: &str = "petstore-31";

/// The test that goes into the example crate, written by hand against the crate generated from
/// [`SUBJECT`], which it imports by that crate's library name.
fn test_fixture() -> Utf8PathBuf {
    crate::paths::corpus_root().join("example/both_halves.rs")
}

pub fn run(args: &Args) -> eyre::Result<()> {
    crate::generated::require_cargo()?;

    let documents = crate::corpus::selected(&[SUBJECT.to_owned()])?;
    let (spec, bytes) = documents
        .first()
        .wrap_err("the subject resolves to itself")?;

    let mut config = crate::corpus::config_for(spec);
    if let Some(choice) = args.serde {
        config.serde_impl = choice.into();
    }
    // The paged listing, declared the way a consumer would declare it.
    config.pagination.insert(
        "list_pets_paged".to_owned(),
        progeny::Pagination {
            cursor_param: "cursor".to_owned(),
            next_cursor: "next".to_owned(),
            items: "items".to_owned(),
        },
    );
    // The JSON-declared photo `label` keeps all three presence states, so the round trip shows an
    // explicit `null` arriving as one rather than as an absent member.
    config
        .nullability_overrides
        .insert(LABEL_POINTER.to_owned(), progeny::SchemaType::String);
    let keyed = with_json_declared_member(&with_string_union_member(&with_streamed_bytes(
        &with_problem_arms(&with_response_headers(&with_security_schemes(bytes)?)?)?,
    )?)?)?;
    let output = progeny::generate(&keyed, &config).wrap_err("generating the example crate")?;
    let directory = crate::generated::write("example-petstore", &output)?;

    let test = test_fixture();
    let source = std::fs::read_to_string(&test).wrap_err_with(|| format!("reading {test}"))?;
    crate::generated::write_wire_test(&directory, "both_halves.rs", &source)?;

    println!("example: {SUBJECT}, both halves, at {directory}");
    if args.generate_only {
        println!("  written but not run: {directory}/tests/both_halves.rs");
        return Ok(());
    }

    let run = crate::generated::cargo(&directory)
        .args([
            "test",
            "--quiet",
            "--all-features",
            "--test",
            "both_halves",
            "--",
            "--nocapture",
        ])
        .output()
        .wrap_err("running the example test")?;
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    if !run.status.success() {
        bail!(
            "{}",
            indoc::formatdoc! {"
                the generated client and server did not agree:
                {output}"
            }
        );
    }
    println!("{}", output.trim());
    println!();
    println!("example: the generated client and the generated server agree on the request line");
    Ok(())
}

/// The subject with one security scheme of every type progeny sends, which one operation
/// requires as alternatives.
///
/// Spliced in here rather than written into the committed document, which every other gate
/// fingerprints: the schemes are this harness's question alone.
/// `showPetById` requires a credential and `listPets` requires nothing, so one client shows each
/// credential sent where it is required and nowhere else.
fn with_security_schemes(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const SCHEMES: &str = indoc::indoc! {"
        securitySchemes:
          petKey: {type: apiKey, in: header, name: X-Pet-Key}
          queryKey: {type: apiKey, in: query, name: api_key}
          cookieKey: {type: apiKey, in: cookie, name: session}
          bearer: {type: http, scheme: bearer}
          basic: {type: http, scheme: basic}
          oauth:
            type: oauth2
            flows:
              clientCredentials:
                tokenUrl: https://example.invalid/token
                scopes: {'pets:read': read}
    "};
    // The query key goes with the cookie key or not at all, so an alternative met in part is
    // shown passed over.
    const REQUIREMENT: &str = indoc::indoc! {"
        security:
          - petKey: []
          - queryKey: []
            cookieKey: []
          - bearer: []
          - basic: []
          - oauth: ['pets:read']
    "};
    spliced(
        bytes,
        &[
            Insertion {
                after: &["components:\n"],
                text: SCHEMES,
                indent: "  ",
            },
            Insertion {
                after: &["      operationId: showPetById\n"],
                text: REQUIREMENT,
                indent: "      ",
            },
        ],
    )
}

/// The subject with response headers on four arms, which no other gate needs either, and two
/// operations whose generated type names would meet.
///
/// `createPets` answers `201` with typed headers of every shape the server types, beside a
/// deprecated one, the `Content-Type` OpenAPI ignores and the `Content-Length` the transport
/// owns; `typedErrors` answers a `204` whose headers are all it says, one of them a value its
/// schema says nothing about; and `showPetById` declares one on its `200` and one on its
/// `default`, the two arms a handler defect turns into a `500` without them.
/// `namedArms` refuses with a `400` whose payload would be `NamedArmsBadRequest`, the request
/// builder of `namedArmsBad`.
fn with_response_headers(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const CREATED: &str = indoc::indoc! {"
        headers:
          Location:
            description: Where the new pet is.
            required: true
            schema: {type: string}
          Retry-After:
            schema: {type: integer, minimum: 0}
          X-Pet-Tags:
            schema: {type: array, items: {type: string}}
          X-Legacy-Id:
            deprecated: true
            schema: {type: integer}
          Content-Type:
            schema: {type: string}
          Content-Length:
            schema: {type: integer}
    "};
    const OUTCOME: &str = indoc::indoc! {"
        headers:
          X-Outcome:
            schema: {type: string}
          X-Flag:
            description: Anything at all, which travels as its text.
            schema: {}
    "};
    const NAMED: &str = indoc::indoc! {"
        /names/{kind}:
          get:
            operationId: namedArms
            summary: Refuse with a header on an arm named like another operation's type
            parameters:
              - name: kind
                in: path
                required: true
                schema: {type: string}
            responses:
              \"204\":
                description: Nothing refused
              \"400\":
                description: Refused
                headers:
                  X-Reason:
                    schema: {type: string}
        /names-bad:
          get:
            operationId: namedArmsBad
            summary: The operation whose request builder the refusal would be named like
            responses:
              \"204\":
                description: Done
    "};
    const NOTE: &str = indoc::indoc! {"
        headers:
          X-Pet-Note:
            schema: {type: string}
    "};
    const REQUEST_ID: &str = indoc::indoc! {"
        headers:
          X-Request-Id:
            schema: {type: string}
    "};
    let arm = "          description:";
    spliced(
        bytes,
        &[
            Insertion {
                after: &["\npaths:\n"],
                text: NAMED,
                indent: "  ",
            },
            Insertion {
                after: &[&format!("{arm} Null response\n")],
                text: CREATED,
                indent: "          ",
            },
            Insertion {
                after: &[&format!("{arm} No error requested\n")],
                text: OUTCOME,
                indent: "          ",
            },
            Insertion {
                after: &[&format!("{arm} Expected response to a valid request\n")],
                text: NOTE,
                indent: "          ",
            },
            // Every operation's `default` reads alike, so this one is found after its operation.
            Insertion {
                after: &[
                    "operationId: showPetById\n",
                    "        default:\n",
                    &format!("{arm} unexpected error\n"),
                ],
                text: REQUEST_ID,
                indent: "          ",
            },
        ],
    )
}

/// The subject with an operation whose error arms all answer the `Error` schema, which no other
/// gate needs.
///
/// `problemArms` answers the status its path names, through the `ErrorResponse` constructor its
/// response enum gets: `400` and `404` exactly, the `404` with a typed header, `503` exactly,
/// `4XX` for any other `4xx`, and `default` for anything else.
/// `rangeArms` declares the two error classes and `default` alone.
fn with_problem_arms(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const PROBLEMS: &str = indoc::indoc! {"
        /problems/{status}:
          get:
            operationId: problemArms
            summary: Answer the error arm a status selects
            parameters:
              - name: status
                in: path
                required: true
                schema: {type: integer, format: int32}
            responses:
              \"204\":
                description: No problem
              \"400\":
                description: Bad request
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              \"404\":
                description: Missing
                headers:
                  X-Problem:
                    schema: {type: string}
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              \"4XX\":
                description: Any other client error
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              \"503\":
                description: Unavailable
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              default:
                description: Anything else
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
        /ranges/{status}:
          get:
            operationId: rangeArms
            summary: Answer the error arm a status selects among ranges alone
            parameters:
              - name: status
                in: path
                required: true
                schema: {type: integer, format: int32}
            responses:
              \"4XX\":
                description: A client error
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              \"5XX\":
                description: A server error
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
              default:
                description: Anything else
                content:
                  application/json:
                    schema: {$ref: \"#/components/schemas/Error\"}
    "};
    spliced(
        bytes,
        &[Insertion {
            after: &["\npaths:\n"],
            text: PROBLEMS,
            indent: "  ",
        }],
    )
}

/// The subject with a binary response that declares a header, which no other gate needs.
///
/// `streamBytes` answers the body its path names: `whole`, bytes held in memory, `chunks`, a
/// stream of three chunks, and `broken`, a stream that fails after its first chunk.
fn with_streamed_bytes(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const STREAM: &str = indoc::indoc! {"
        /stream/{mode}:
          get:
            operationId: streamBytes
            summary: Answer a binary body the way the mode says
            parameters:
              - name: mode
                in: path
                required: true
                schema: {type: string}
            responses:
              \"200\":
                description: The bytes
                headers:
                  X-Chunks:
                    schema: {type: integer}
                content:
                  application/octet-stream:
                    schema: {type: string, format: binary}
    "};
    spliced(
        bytes,
        &[Insertion {
            after: &["\npaths:\n"],
            text: STREAM,
            indent: "  ",
        }],
    )
}

/// The subject with a photo `subject` that is a union of strings, which no other gate needs.
///
/// A union of strings is a string on the wire, so the client sends it as a text part and the
/// server keeps its text; one that spells a number is still the string.
fn with_string_union_member(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const SUBJECT: &str = indoc::indoc! {"
        subject:
          description: What the photo is filed under.
          oneOf:
            - {type: string, pattern: '^[0-9a-z_]+$'}
            - {type: string, const: submission}
    "};
    spliced(
        bytes,
        &[Insertion {
            after: &[
                "operationId: uploadPetPhoto\n",
                "                  description: An optional numeric rating.\n",
                "                  format: int32\n",
            ],
            text: SUBJECT,
            indent: "                ",
        }],
    )
}

/// Where [`with_json_declared_member`] puts the photo `label`, which the run's configuration
/// overrides to keep its presence states.
const LABEL_POINTER: &str = "/paths/~1pets~1{petId}~1photo/post/requestBody/content/multipart~1form-data/schema/properties/label";

/// The subject with a photo `label` that is a plain string whose encoding declares JSON, which no
/// other gate needs.
///
/// The part's header says JSON, so its content is the string's JSON, quoted, and an explicit
/// `null` is the JSON `null`.
fn with_json_declared_member(bytes: &[u8]) -> eyre::Result<Vec<u8>> {
    const LABEL: &str = indoc::indoc! {"
        label:
          description: A caption, sent as JSON.
          type: string
    "};
    const ENCODING: &str = indoc::indoc! {"
        encoding:
          label:
            contentType: application/json
    "};
    spliced(
        bytes,
        &[
            Insertion {
                after: &[
                    "operationId: uploadPetPhoto\n",
                    "          multipart/form-data:\n",
                ],
                text: ENCODING,
                indent: "            ",
            },
            Insertion {
                after: &[
                    "operationId: uploadPetPhoto\n",
                    "                  description: An optional numeric rating.\n",
                    "                  format: int32\n",
                ],
                text: LABEL,
                indent: "                ",
            },
        ],
    )
}

/// YAML lines to add to the subject.
struct Insertion<'a> {
    /// Lines found one after another, the first anywhere and each later one after the last; the
    /// text goes right after the final one.
    after: &'a [&'a str],
    text: &'a str,
    /// What every line of `text` is indented by, which places it in the mapping it extends.
    indent: &'a str,
}

/// The subject with each insertion indented and placed where its anchor lines lead.
fn spliced(bytes: &[u8], insertions: &[Insertion<'_>]) -> eyre::Result<Vec<u8>> {
    let text = std::str::from_utf8(bytes).wrap_err("the subject is UTF-8")?;
    let mut keyed = text.to_owned();
    for Insertion {
        after,
        text,
        indent,
    } in insertions
    {
        let mut at = 0;
        for anchor in *after {
            at += keyed
                .get(at..)
                .and_then(|rest| rest.find(anchor))
                .wrap_err_with(|| format!("the subject no longer has `{}`", anchor.trim()))?
                + anchor.len();
        }
        let mut indented = String::new();
        for line in text.lines() {
            let _ = writeln!(indented, "{indent}{line}");
        }
        keyed.insert_str(at, &indented);
    }
    Ok(keyed.into_bytes())
}
