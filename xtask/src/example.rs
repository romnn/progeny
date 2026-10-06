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
//!
//! Its subject is the committed `petstore-31`, which needs no network and whose `rejectionProbe`
//! operation carries a path parameter, a query parameter, a header, a cookie and a body at once.

use std::fmt::Write as _;

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
    let keyed = with_security_schemes(bytes)?;
    let output = progeny::generate(&keyed, &config).wrap_err("generating the example crate")?;
    let directory = crate::generated::write("example-petstore", &output)?;

    crate::generated::write_wire_test(
        &directory,
        "both_halves.rs",
        &test_source(&config.package.name),
    )?;

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
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    if !run.status.success() {
        bail!(
            "{}",
            indoc::formatdoc! {"
                the generated client and server did not agree:
                {text}"
            }
        );
    }
    println!("{}", text.trim());
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
    let text = std::str::from_utf8(bytes).wrap_err("the subject is UTF-8")?;
    let mut keyed = text.to_owned();
    for (anchor, insertion, indent) in [
        ("components:\n", SCHEMES, "  "),
        ("      operationId: showPetById\n", REQUIREMENT, "      "),
    ] {
        let at = keyed
            .find(anchor)
            .wrap_err_with(|| format!("the subject no longer has `{}`", anchor.trim()))?
            + anchor.len();
        let mut indented = String::new();
        for line in insertion.lines() {
            let _ = writeln!(indented, "{indent}{line}");
        }
        keyed.insert_str(at, &indented);
    }
    Ok(keyed.into_bytes())
}

/// The test that goes into the example crate.
///
/// Written out as source rather than assembled from the model on purpose: it is the *reader's*
/// check on progeny, and a test generated from the same records it is testing would agree with a
/// mistake in them. This one says what a petstore request looks like in plain Rust, and it is wrong
/// exactly when progeny is.
fn test_source(krate: &str) -> String {
    let krate = crate::corpus::lib_name(krate);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        indoc::indoc! {"
            //! Generated by `cargo xtask example`. The client and the server of one description,
            //! talking over a real socket.
            //!
            //! This is the only assertion in the project that a generated `send()` builds the request
            //! the description asks for: every other gate checks one side of the wire.
        "}
    );
    let _ = writeln!(
        out,
        "{}",
        indoc::formatdoc! {"
            use std::sync::{{Arc, Mutex}};

            use color_eyre::eyre::{{self, OptionExt as _}};
            use {krate}::{{client, operations, server, types}};
        "}
    );
    out.push_str(RECORDER);
    out.push_str(IMPLEMENTATION);
    out.push_str(TESTS);
    out
}

/// The test double: an `Api` implementation that records what each handler was handed.
const RECORDER: &str = indoc::indoc! {r"
/// What the server saw, so the test can assert on the request rather than only on the reply.
/// Field-by-field rather than holding a `types::Pet`: a generated type carries only the derives
/// the configuration asked for, and `PartialEq` is not one of them by default. Comparing what the
/// server saw against what the client sent is the point, and it does not need the type to be
/// comparable.
#[derive(Debug, Default, Clone, PartialEq)]
struct Seen {
    limit: Option<i64>,
    pet_id: Option<String>,
    probe_id: Option<i64>,
    probe_limit: Option<i64>,
    probe_mode: Option<String>,
    probe_session: Option<i64>,
    probe_body: Option<(i64, String)>,
    photo_note: Option<String>,
    photo_file: Option<String>,
}

/// Cloneable rather than shared behind an `Arc` from outside, because `router` takes the
/// implementation by value and a foreign trait cannot be implemented for `Arc<T>` here anyway.
#[derive(Debug, Default, Clone)]
struct Double {
    seen: Arc<Mutex<Seen>>,
}

impl Double {
    fn record(&self, edit: impl FnOnce(&mut Seen)) {
        let mut seen = match self.seen.lock() {
            Ok(seen) => seen,
            Err(poisoned) => poisoned.into_inner(),
        };
        edit(&mut seen);
    }

    fn seen(&self) -> Seen {
        match self.seen.lock() {
            Ok(seen) => seen.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}
"};

/// The `Api` implementation. Spelled out rather than generated, so it reads as a consumer's would.
const IMPLEMENTATION: &str = indoc::indoc! {r#"
impl server::Api for Double {
    async fn list_pets(&self, query: server::ListPetsQuery) -> server::ListPetsResponse {
        self.record(|seen| seen.limit = query.limit);
        // A response position takes the read form of its type. A strict value converts into
        // it, which is how a server built on the strict types answers.
        server::ListPetsResponse::Ok(Box::new(vec![types::Pet {
            id: 1,
            name: "Rex".to_owned(),
            tag: Some("dog".to_owned()),
        }
        .into()]))
    }

    async fn create_pets(&self) -> server::CreatePetsResponse {
        server::CreatePetsResponse::Created(())
    }

    async fn list_pets_paged(
        &self,
        query: server::ListPetsPagedQuery,
    ) -> server::ListPetsPagedResponse {
        // Two pages: the first names a cursor, the second does not.
        let (items, next) = match query.cursor.as_deref() {
            None => (vec![(1, "Rex"), (2, "Tom")], Some("page-2".to_owned())),
            Some(_) => (vec![(3, "Kit")], None),
        };
        server::ListPetsPagedResponse::Ok(Box::new(types::read::PetPage {
            items: Some(
                items
                    .into_iter()
                    .map(|(id, name)| types::read::Pet {
                        id: Some(id),
                        name: Some(name.to_owned()),
                        ..Default::default()
                    })
                    .collect(),
            ),
            next,
            ..Default::default()
        }))
    }

    async fn show_pet_by_id(&self, path: server::ShowPetByIdPath) -> server::ShowPetByIdResponse {
        self.record(|seen| seen.pet_id = Some(path.pet_id.clone()));
        server::ShowPetByIdResponse::Ok(Box::new(types::read::Pet::from(types::Pet {
            id: 7,
            name: path.pet_id,
            tag: None,
        })))
    }

    async fn upload_pet_photo(
        &self,
        path: server::UploadPetPhotoPath,
        body: types::UploadPetPhotoBody,
    ) -> server::UploadPetPhotoResponse {
        self.record(|seen| {
            seen.photo_note = body.note.clone();
            seen.photo_file = Some(body.file.clone());
        });
        // A type only a response yields has no strict form; its read form is written out, and
        // `Default` fills whatever the answer leaves unsaid.
        server::UploadPetPhotoResponse::Created(Box::new(types::read::PhotoUpload {
            pet_id: Some(path.pet_id),
            filename: Some("photo".to_owned()),
            content_type: Some("image/png".to_owned()),
            file_size: Some(body.file.len() as i64),
            note: body.note,
            rating: body.rating,
            ..Default::default()
        }))
    }

    async fn typed_errors(&self, path: server::TypedErrorsPath) -> server::TypedErrorsResponse {
        match path.kind.as_str() {
            "missing" => server::TypedErrorsResponse::NotFound(Box::new(types::read::Error {
                code: Some(404),
                message: Some("missing".to_owned()),
                ..Default::default()
            })),
            "conflict" => server::TypedErrorsResponse::Conflict(Box::new(types::read::Error {
                code: Some(409),
                message: Some("conflict".to_owned()),
                ..Default::default()
            })),
            _ => server::TypedErrorsResponse::NoContent(()),
        }
    }

    async fn download_bytes(&self) -> server::DownloadBytesResponse {
        server::DownloadBytesResponse::Ok(Box::new(vec![0, 255, b'{', b'\n']))
    }

    async fn download_text(&self) -> server::DownloadTextResponse {
        server::DownloadTextResponse::Ok(Box::new(
            "plain text\nthat is not JSON".to_owned(),
        ))
    }

    async fn rejection_probe(
        &self,
        path: server::RejectionProbePath,
        query: server::RejectionProbeQuery,
        header: server::RejectionProbeHeader,
        cookie: server::RejectionProbeCookie,
        body: types::Pet,
    ) -> server::RejectionProbeResponse {
        self.record(|seen| {
            seen.probe_id = Some(path.id);
            seen.probe_limit = Some(query.limit);
            seen.probe_mode = Some(header.x_mode.clone());
            seen.probe_session = Some(cookie.session);
            seen.probe_body = Some((body.id, body.name.clone()));
        });
        server::RejectionProbeResponse::NoContent(())
    }
}
"#};

/// The assertions themselves.
const TESTS: &str = indoc::indoc! {r#"
/// Start the generated server on an ephemeral port and hand back a client pointed at it.
async fn serving() -> eyre::Result<(Double, client::Client)> {
    let double = Double::default();
    let router = server::router(double.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((double, client::Client::new(format!("http://{address}"))))
}

/// Every credential a request carried, each named after where it travelled, or `none`.
fn saw_credentials(headers: &axum::http::HeaderMap, query: Option<&str>) -> String {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let seen: Vec<String> = [
        ("key", header("x-pet-key")),
        ("query", query),
        ("cookie", header("cookie")),
        ("auth", header("authorization")),
    ]
    .into_iter()
    .filter_map(|(place, value)| value.map(|value| format!("{place}={value}")))
    .collect();
    if seen.is_empty() {
        "none".to_owned()
    } else {
        seen.join(" ")
    }
}

/// Start a hand-written raw server so the generated client cannot agree with its own renderer.
async fn raw_serving() -> eyre::Result<client::Client> {
    use axum::http::header::CONTENT_TYPE;
    use axum::routing::get;

    let router = axum::Router::new()
        .route(
            "/download",
            get(|| async {
                (
                    [(CONTENT_TYPE, "application/octet-stream")],
                    vec![0, 255, b'{', b'\n'],
                )
            }),
        )
        .route(
            "/motd",
            get(|| async {
                (
                    [(CONTENT_TYPE, "text/plain")],
                    "plain text\nthat is not JSON",
                )
            }),
        )
        // A vendor that drifted: a required member sent as `null`, a member the description
        // never declared, and a required member missing from one record of the page.
        // It also says which credentials the request carried, which `listPets` never requires.
        .route(
            "/pets",
            get(|headers: axum::http::HeaderMap, axum::extract::RawQuery(query): axum::extract::RawQuery| async move {
                (
                    [("x-saw-credentials", saw_credentials(&headers, query.as_deref()))],
                    axum::Json(serde_json::json!([
                        {"id": 1, "name": null, "tag": "dog", "color": "brown"},
                        {"id": 2, "tag": "cat"},
                        {"id": 3, "name": "Rex"},
                    ])),
                )
            }),
        )
        // A paged listing whose second page has an item that cannot be read: the stream must
        // not skip it quietly.
        .route(
            "/pets/paged",
            get(|query: axum::extract::Query<std::collections::BTreeMap<String, String>>| async move {
                if query.contains_key("cursor") {
                    axum::Json(serde_json::json!({"items": [{"id": 3, "name": "Kit"}, "??"]}))
                } else {
                    axum::Json(serde_json::json!({
                        "items": [{"id": 1, "name": "Rex"}, {"id": 2, "name": "Tom"}],
                        "next": "page-2",
                    }))
                }
            }),
        )
        // `keyed` answers with the credentials the request carried, `chunked` with a body of no
        // declared length, and anything else with a root that is not what the description
        // declares at all.
        .route(
            "/pets/{pet_id}",
            get(
                |axum::extract::Path(pet_id): axum::extract::Path<String>,
                 axum::extract::RawQuery(query): axum::extract::RawQuery,
                 headers: axum::http::HeaderMap| async move {
                    use axum::response::IntoResponse as _;
                    match pet_id.as_str() {
                        "keyed" => axum::Json(serde_json::json!({
                            "id": 7,
                            "name": saw_credentials(&headers, query.as_deref()),
                        }))
                        .into_response(),
                        "chunked" => axum::body::Body::from_stream(futures_util::stream::iter([
                            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(
                                vec![b' '; 64],
                            )),
                        ]))
                        .into_response(),
                        _ => axum::Json(serde_json::json!(["not", "an", "object"])).into_response(),
                    }
                },
            ),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(client::Client::new(format!("http://{address}")))
}

#[test_util::test]
async fn a_query_parameter_survives_the_round_trip() {
    let (double, client) = serving().await?;
    let pets = client
        .list_pets(client::ListPetsParams { limit: Some(3) })
        .send()
        .await?;
    assert_eq!(double.seen().limit, Some(3));
    assert!(!pets.is_degraded(), "{}", pets.degradations());
    assert_eq!(pets.into_value()[0].name.as_deref(), Some("Rex"));

    // And an unset optional parameter arrives unset, rather than as a default the caller never
    // chose — the same rule the client's params and the server's extractor have to agree on.
    let (double, client) = serving().await?;
    let _ = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await?;
    assert_eq!(double.seen().limit, None);
}

#[test_util::test]
async fn a_path_parameter_survives_the_round_trip() {
    let (double, client) = serving().await?;
    let pet = client
        .show_pet_by_id(client::ShowPetByIdParams {
            pet_id: "a pet/with slashes".to_owned(),
        })
        .send()
        .await?;
    // Percent-encoded on the way out and decoded on the way in, which is the pair of rules a
    // template variable actually depends on.
    assert_eq!(double.seen().pet_id.as_deref(), Some("a pet/with slashes"));
    assert_eq!(pet.into_value().name.as_deref(), Some("a pet/with slashes"));
}

#[test_util::test]
async fn every_parameter_location_arrives_at_once() {
    // The one operation with a path parameter, a query parameter, a header, a cookie and a body
    // together. Four locations is where a renderer that puts one in the wrong place still passes
    // every other gate.
    let (double, client) = serving().await?;
    let sent = types::Pet {
        id: 42,
        name: "Probe".to_owned(),
        tag: None,
    };
    let expected = (sent.id, sent.name.clone());
    client
        .rejection_probe(client::RejectionProbeParams {
            id: 9,
            limit: 11,
            x_mode: "strict".to_owned(),
            session: 1234,
            body: sent,
        })
        .send()
        .await?;
    let seen = double.seen();
    assert_eq!(seen.probe_id, Some(9));
    assert_eq!(seen.probe_limit, Some(11));
    assert_eq!(seen.probe_mode.as_deref(), Some("strict"));
    assert_eq!(seen.probe_session, Some(1234));
    assert_eq!(seen.probe_body, Some(expected));
}

#[test_util::test]
async fn a_multipart_body_survives_the_round_trip() {
    // The writer and the reader are the same rule read from two ends, and this is the only place
    // both ends actually run.
    let (double, client) = serving().await?;
    let reply = client
        .upload_pet_photo(client::UploadPetPhotoParams {
            pet_id: 5,
            body: types::UploadPetPhotoBody {
                file: "not really a png".to_owned(),
                note: Some("a note".to_owned()),
                rating: Some(4),
            },
        })
        .send()
        .await?;
    let seen = double.seen();
    assert_eq!(seen.photo_file.as_deref(), Some("not really a png"));
    assert_eq!(seen.photo_note.as_deref(), Some("a note"));
    assert_eq!(reply.into_value().pet_id, Some(5));
}

#[test_util::test]
async fn a_declared_error_status_arrives_as_the_typed_error_it_was_sent_as() {
    // The client's status match and the server's response enum come from one record, and this is
    // the assertion that they agree about which arm a status lands in.
    let (_, client) = serving().await?;
    let error = client
        .typed_errors(client::TypedErrorsParams {
            kind: "conflict".to_owned(),
        })
        .send()
        .await
        .err()
        .ok_or_eyre("a declared failure")?;
    let message = format!("{error:?}");
    assert!(message.contains("409"), "{message}");
    assert!(message.contains("conflict"), "{message}");

    let (_, client) = serving().await?;
    client
        .typed_errors(client::TypedErrorsParams {
            kind: "neither".to_owned(),
        })
        .send()
        .await?;
}

#[test_util::test]
async fn the_client_reads_non_json_response_bodies_from_the_raw_wire() {
    let client = raw_serving().await?;

    let bytes = client.download_bytes().send().await?.into_value();
    assert_eq!(bytes, [0, 255, b'{', b'\n']);

    let text = client.download_text().send().await?.into_value();
    assert_eq!(text, "plain text\nthat is not JSON");
}

/// The Hypofy case: a payload the description no longer matches decodes as far as it allows,
/// with every deviation reported by its place in the description rather than failing the call.
#[test_util::test]
async fn a_drifted_response_decodes_as_far_as_it_can_and_says_what_it_tolerated() {
    let client = raw_serving().await?;
    let pets = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await?;
    assert!(pets.is_degraded());
    let report = pets.degradations().clone();
    let pets = pets.into_value();
    assert_eq!(pets.len(), 3);
    assert_eq!(pets[0].name, None);
    assert_eq!(pets[0].tag.as_deref(), Some("dog"));
    assert_eq!(pets[0].extra.get("color"), Some(&serde_json::json!("brown")));
    assert_eq!(pets[1].name, None);
    assert_eq!(pets[2].name.as_deref(), Some("Rex"));

    // One entry per (site, kind), however many records shared the drift, each naming the
    // pointer an override would use.
    // Spelled from the type's own constants: nothing copied out of the generated source, and
    // nothing a consumer could construct wrong.
    let name = types::read::Pet::SITE_NAME;
    assert_eq!(name.origin(), "/components/schemas/Pet");
    assert_eq!(name.member(), Some("name"));
    let kinds: Vec<types::DegradationKind> = report.at(name).map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [types::DegradationKind::RequiredAbsent, types::DegradationKind::NullNotAllowed]
    );
    // The member's entries belong to the type; the type's own site holds only what the type
    // itself tolerated, which here is the member the description never declared.
    assert!(report.within(types::read::Pet::SITE).count() > kinds.len());
    assert!(!report.touches(types::read::Pet::SITE_TAG));
    let undeclared = report
        .iter()
        .find(|entry| entry.kind == types::DegradationKind::UndeclaredMember)
        .ok_or_eyre("the undeclared member is reported")?;
    assert_eq!(undeclared.site, types::read::Pet::SITE);
    assert_eq!(undeclared.site.type_name(), "Pet");
    assert_eq!(undeclared.samples, ["color"]);
    let rendered = report.to_string();
    assert!(rendered.contains("/components/schemas/Pet/properties/name"), "{rendered}");
    assert_eq!(report.len(), 3, "{rendered}");
    // A client's report is keyed at the operation's response, which is no type's site.
    assert_eq!(report.root().type_name(), "list_pets");
    assert_eq!(report.root().origin(), "/paths/~1pets/get/responses/200");

    // A payload that is not the declared shape at the root has no value to hand back.
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "1".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("a root of the wrong shape is refused")?;
    assert!(matches!(error, client::Error::Decode(_)), "{error:?}");

    // The same read type through plain serde: the same value, without the report.
    let parsed: types::read::Pet = serde_json::from_str("{\"id\": 4, \"extra\": true}")?;
    assert_eq!(parsed.id, Some(4));
    assert_eq!(parsed.name, None);
    assert_eq!(parsed.extra.get("extra"), Some(&serde_json::json!(true)));
    // And written back as it arrived: no invented `null` for what was missing.
    assert_eq!(serde_json::to_string(&parsed)?, "{\"id\":4,\"extra\":true}");

    // And with the report kept, outside a client: keyed at the type the payload is.
    let decoded = types::Decoded::<types::read::Pet>::from_json("{\"id\": 4, \"extra\": true}")?;
    assert_eq!(decoded.value.id, Some(4));
    assert_eq!(decoded.degradations.root(), types::read::Pet::SITE);
    assert!(decoded.is_degraded(), "an undeclared member is drift");
    assert!(decoded.degradations.touches(types::read::Pet::SITE));
    // A list of them is keyed at the element type, and a body that goes on after its value is
    // not a JSON document whichever way it is read.
    let page = types::Decoded::<Vec<types::read::Pet>>::from_json("[{\"id\": 4}]")?;
    assert_eq!(page.degradations.root(), types::read::Pet::SITE);
    assert!(types::Decoded::<types::read::Pet>::from_json("{} tail").is_err());
}

/// A stream follows the cursor through read forms, and a page degraded on the path it walks —
/// here an item that cannot be read — ends the stream with an error rather than skipping.
#[test_util::test]
async fn a_stream_walks_read_forms_and_refuses_a_degraded_page() {
    use futures_util::TryStreamExt as _;

    let (_, client) = serving().await?;
    let names: Vec<String> = client
        .list_pets_paged(client::ListPetsPagedParams { cursor: None })
        .stream()
        .map_ok(|pet| pet.name.unwrap_or_default())
        .try_collect()
        .await?;
    assert_eq!(names, ["Rex", "Tom", "Kit"]);

    let client = raw_serving().await?;
    let mut stream = std::pin::pin!(
        client
            .list_pets_paged(client::ListPetsPagedParams { cursor: None })
            .stream()
    );
    let mut names = Vec::new();
    let error = loop {
        match stream.try_next().await {
            Ok(Some(pet)) => names.push(pet.name.unwrap_or_default()),
            Ok(None) => eyre::bail!("the degraded page should have ended the stream"),
            Err(error) => break error,
        }
    };
    assert_eq!(names, ["Rex", "Tom"]);
    assert_eq!(error.status().map(|status| status.as_u16()), Some(200));
    let client::Error::DegradedPage(page) = error else {
        eyre::bail!("expected a degraded page, got {error:?}");
    };
    // The page comes back whole with its body set aside: the status and headers it arrived
    // with, and the report that refused it.
    assert_eq!(page.status(), 200);
    assert!(page.is_degraded());
    let rendered = page.degradations().to_string();
    assert!(rendered.contains("PetPage.items"), "{rendered}");
    assert!(rendered.contains("could not be read"), "{rendered}");
}

/// The observer sees every degraded response once, with the operation it answered and the same
/// report the response carries; a clean response never reaches it.
#[test_util::test]
async fn an_observer_is_told_about_every_degraded_response() {
    // The operation is the reflection module's own enum, so an observer keying metrics by
    // operation matches on a type rather than on a string the client happened to spell.
    let seen: Arc<Mutex<Vec<(operations::Operation, u16, String)>>> = Arc::default();
    let client = raw_serving().await?.observe({
        let seen = Arc::clone(&seen);
        move |degraded: client::Degraded<'_>| {
            if let Ok(mut seen) = seen.lock() {
                seen.push((
                    degraded.operation,
                    degraded.status.as_u16(),
                    degraded.degradations.to_string(),
                ));
            }
        }
    });
    let pets = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await?;
    let text = client.download_text().send().await?;
    assert!(!text.is_degraded());
    let seen = seen.lock().map_err(|_| eyre::eyre!("poisoned"))?.clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, operations::Operation::ListPets);
    assert_eq!(seen[0].0.rust_name(), "list_pets");
    assert_eq!(seen[0].1, 200);
    assert_eq!(seen[0].2, pets.degradations().to_string());
}

#[test_util::test]
async fn the_server_writes_declared_non_json_bodies_and_content_types() {
    let (_, client) = serving().await?;
    let raw = reqwest::Client::new();

    let bytes = raw
        .get(format!("{}/download", client.base_url()))
        .send()
        .await?;
    assert_eq!(
        bytes.headers().get(reqwest::header::CONTENT_TYPE),
        Some(&reqwest::header::HeaderValue::from_static(
            "application/octet-stream"
        ))
    );
    assert_eq!(bytes.bytes().await?.as_ref(), [0, 255, b'{', b'\n']);

    let text = raw
        .get(format!("{}/motd", client.base_url()))
        .send()
        .await?;
    assert_eq!(
        text.headers().get(reqwest::header::CONTENT_TYPE),
        Some(&reqwest::header::HeaderValue::from_static("text/plain"))
    );
    assert_eq!(text.text().await?, "plain text\nthat is not JSON");
}

#[test_util::test]
async fn a_request_the_description_does_not_describe_is_rejected_in_one_place() {
    // Not sent through the generated client, because the generated client cannot build a request
    // this wrong — which is the point of the params struct. The rejection envelope exists for
    // every *other* client, and this is what it answers with.
    let (_, client) = serving().await?;
    let raw = reqwest::Client::new();
    let response = raw
        .post(format!("{}/rejection-probe/9?limit=11", client.base_url()))
        .header("x-mode", "strict")
        .json(&types::Pet { id: 1, name: "p".to_owned(), tag: None })
        .send()
        .await?;
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await?;
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("session"), "{body}");
    assert!(message.contains("rejection_probe"), "{body}");
}

/// A body past the limit is refused, whether its length was declared up front or only found
/// while reading; the request's own limit overrides the client's.
#[test_util::test]
async fn a_body_past_the_limit_is_refused_before_it_is_held_whole() {
    let client = raw_serving().await?.response_body_limit(16);

    // Declared: the drifted listing is longer than sixteen bytes and says so in its length.
    let error = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await
        .err()
        .ok_or_eyre("a declared length past the limit is refused")?;
    let client::Error::BodyTooLarge(refused) = &error else {
        eyre::bail!("expected BodyTooLarge, got {error:?}");
    };
    assert_eq!(refused.limit(), 16);
    assert_eq!(error.status().map(|status| status.as_u16()), Some(200));

    // Chunked: no length is declared, so the limit is found while reading.
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "chunked".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("a chunked body past the limit is refused")?;
    assert!(matches!(error, client::Error::BodyTooLarge(_)), "{error:?}");

    // The request's own limit wins over the client's.
    let pets = client
        .list_pets(client::ListPetsParams { limit: None })
        .response_body_limit(1 << 20)
        .send()
        .await?;
    assert_eq!(pets.into_value().len(), 3);
}

/// A request's `None` lifts the client's limit: the body the client would refuse is read whole.
#[test_util::test]
async fn a_request_can_lift_the_client_limit() {
    let client = raw_serving().await?.response_body_limit(16);
    let pets = client
        .list_pets(client::ListPetsParams { limit: None })
        .response_body_limit(None)
        .keep_raw_body()
        .send()
        .await?;
    // Longer than the client's sixteen bytes, and every byte of it read.
    let raw = pets.raw_body().ok_or_eyre("the body was kept")?;
    assert!(raw.len() > 16, "{}", raw.len());
    assert_eq!(pets.into_value().len(), 3);

    // A client's `None` sets no limit, as never calling it does.
    let unlimited = raw_serving().await?.response_body_limit(None);
    let pets = unlimited
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await?;
    assert_eq!(pets.into_value().len(), 3);
}

/// A kept body is the bytes the server sent, with what the read form cannot hold: the member
/// the description never declared and the `null` the decoder read as absent.
#[test_util::test]
async fn a_kept_raw_body_is_what_the_server_sent() {
    let client = raw_serving().await?;
    let pets = client
        .list_pets(client::ListPetsParams { limit: None })
        .keep_raw_body()
        .send()
        .await?;
    let raw: serde_json::Value =
        serde_json::from_slice(pets.raw_body().ok_or_eyre("the body was kept")?)?;
    assert_eq!(raw[0]["color"], serde_json::json!("brown"));
    assert_eq!(raw[0].get("name"), Some(&serde_json::Value::Null));
    // The value is decoded as ever beside it.
    assert_eq!(pets.value().len(), 3);
    assert!(pets.is_degraded());

    // Without asking, nothing is kept.
    let plain = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await?;
    assert!(plain.raw_body().is_none());

    // A declared error keeps its body too.
    let (_, client) = serving().await?;
    let error = client
        .typed_errors(client::TypedErrorsParams { kind: "conflict".to_owned() })
        .keep_raw_body()
        .send()
        .await
        .err()
        .ok_or_eyre("a declared failure")?;
    let client::Error::Declared(response) = error else {
        eyre::bail!("expected a declared error, got {error:?}");
    };
    let raw = String::from_utf8(response.into_raw_body().ok_or_eyre("the body was kept")?)?;
    assert!(raw.contains("conflict"), "{raw}");
}

/// An empty path value would address the collection rather than one pet, so it is refused
/// before anything is sent.
#[test_util::test]
async fn an_empty_path_value_is_refused_before_the_request_exists() {
    let client = raw_serving().await?;
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: String::new() })
        .send()
        .await
        .err()
        .ok_or_eyre("an empty segment is refused")?;
    assert!(
        matches!(&error, client::Error::UnsendablePath { parameter: "petId", rendered } if rendered.is_empty()),
        "{error:?}"
    );
}

/// Each credential goes only where the description requires it, in its own place and format,
/// and the client never spells one in its `Debug` output.
#[test_util::test]
async fn each_credential_is_sent_only_where_it_is_required() {
    use reqwest::header::HeaderValue;

    /// What the server saw on the operation that requires a credential, and on one that does
    /// not.
    async fn seen(client: &client::Client) -> eyre::Result<(String, String)> {
        let pet = client
            .show_pet_by_id(client::ShowPetByIdParams { pet_id: "keyed".to_owned() })
            .send()
            .await?;
        let pets = client
            .list_pets(client::ListPetsParams { limit: None })
            .send()
            .await?;
        let listed = pets
            .headers()
            .get("x-saw-credentials")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        Ok((pet.into_value().name.unwrap_or_default(), listed))
    }

    // Header key
    let client = raw_serving().await?.with_pet_key(HeaderValue::from_static("hunter2"));
    assert_eq!(seen(&client).await?, ("key=hunter2".to_owned(), "none".to_owned()));
    assert!(!format!("{client:?}").contains("hunter2"));

    // Query and cookie keys, which only go together; the cookie value is encoded.
    let client = raw_serving()
        .await?
        .with_query_key("q s")
        .with_cookie_key("c;1");
    assert_eq!(
        seen(&client).await?,
        ("query=api_key=q+s cookie=session=c%3B1".to_owned(), "none".to_owned())
    );
    let debug = format!("{client:?}");
    assert!(!debug.contains("q s") && !debug.contains("c;1"), "{debug}");
    // One without the other meets no alternative, so neither is sent.
    let half = raw_serving().await?.with_query_key("q");
    assert_eq!(seen(&half).await?.0, "none");

    // Bearer
    let client = raw_serving().await?.with_bearer(HeaderValue::from_static("t0ken"));
    assert_eq!(seen(&client).await?, ("auth=Bearer t0ken".to_owned(), "none".to_owned()));

    // Basic, with RFC 7617's example
    let client = raw_serving().await?.with_basic("Aladdin", "open sesame");
    assert_eq!(seen(&client).await?.0, "auth=Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");

    // OAuth2: a bearer token the caller obtained, after the alternatives before it went unmet.
    let client = raw_serving().await?.with_oauth(HeaderValue::from_static("access"));
    assert_eq!(seen(&client).await?.0, "auth=Bearer access");

    // The first alternative met wins, and a header set on the request outranks the client's.
    let client = raw_serving()
        .await?
        .with_pet_key(HeaderValue::from_static("hunter2"))
        .with_bearer(HeaderValue::from_static("t0ken"));
    assert_eq!(seen(&client).await?.0, "key=hunter2");
    let pet = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "keyed".to_owned() })
        .header(
            reqwest::header::HeaderName::from_static("x-pet-key"),
            HeaderValue::from_static("explicit"),
        )
        .send()
        .await?;
    assert_eq!(pet.into_value().name.as_deref(), Some("key=explicit"));

    // Without any credential set, the request that requires one is sent without it.
    assert_eq!(seen(&raw_serving().await?).await?.0, "none");
}

/// A query key is in the URL, which no header marking covers, so the error a failed request
/// returns drops the query before it can print the key.
#[test_util::test]
async fn a_query_key_does_not_reach_an_error_message() {
    // Nothing listens on the discard port, so the request fails before any response.
    // The cookie key is set too, because the query key only goes with it.
    let client = client::Client::new("http://127.0.0.1:9")
        .with_query_key("s3cret")
        .with_cookie_key("c");
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "keyed".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("nothing answers on the discard port")?;
    assert!(matches!(error, client::Error::Request(_)), "{error:?}");
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }
}
"#};
