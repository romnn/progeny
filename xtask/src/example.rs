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
    let keyed = with_response_headers(&with_security_schemes(bytes)?)?;
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
    out.push_str(GATEKEEPER);
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
    probe_caller: Option<String>,
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
    type Context = ();

    async fn list_pets(&self, _context: (), query: server::ListPetsQuery) -> server::ListPetsResponse {
        self.record(|seen| seen.limit = query.limit);
        // A response position takes the read form of its type. A strict value converts into
        // it, which is how a server built on the strict types answers.
        server::ListPetsResponse::Ok {
            headers: Default::default(),
            body: Box::new(vec![types::Pet {
                id: 1,
                name: "Rex".to_owned(),
                tag: Some("dog".to_owned()),
            }
            .into()]),
        }
    }

    async fn create_pets(&self, _context: ()) -> server::CreatePetsResponse {
        // A `raw` value replacing a typed field of the same name, and two raw values of a name
        // the description does not declare, in the order the handler wrote them.
        let mut raw = axum::http::HeaderMap::new();
        raw.insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from_static("soon"),
        );
        for trace in ["first", "second"] {
            raw.append("x-trace", axum::http::HeaderValue::from_static(trace));
        }
        #[expect(
            deprecated,
            reason = "the double still answers with the header the description deprecates"
        )]
        let headers = server::CreatePetsCreatedHeaders {
            location: Some("/pets/7".to_owned()),
            retry_after: Some(30),
            x_pet_tags: Some(vec!["new".to_owned(), "dog".to_owned()]),
            x_legacy_id: Some(7),
            raw,
        };
        server::CreatePetsResponse::Created { headers }
    }

    async fn list_pets_paged(
        &self,
        _context: (),
        query: server::ListPetsPagedQuery,
    ) -> server::ListPetsPagedResponse {
        // Two pages: the first names a cursor, the second does not.
        let (items, next) = match query.cursor.as_deref() {
            None => (vec![(1, "Rex"), (2, "Tom")], Some("page-2".to_owned())),
            Some(_) => (vec![(3, "Kit")], None),
        };
        server::ListPetsPagedResponse::Ok {
            headers: Default::default(),
            body: Box::new(types::read::PetPage {
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
            }),
        }
    }

    async fn show_pet_by_id(
        &self,
        _context: (),
        path: server::ShowPetByIdPath,
    ) -> server::ShowPetByIdResponse {
        self.record(|seen| seen.pet_id = Some(path.pet_id.clone()));
        // A handler defect: `default` answered with the status the `200` arm claims, which a
        // client would decode as a pet.
        if path.pet_id == "misrouted" {
            return server::ShowPetByIdResponse::Default {
                status: 200,
                headers: server::ShowPetByIdDefaultHeaders {
                    x_request_id: Some("r-1".to_owned()),
                    ..Default::default()
                },
                body: Box::new(types::read::Error {
                    code: Some(200),
                    message: Some("not a pet".to_owned()),
                    ..Default::default()
                }),
            };
        }
        // Another: a typed header whose value has a line break, which no HTTP field carries, or
        // whitespace around it, which HTTP strips. A raw value of the same name replaces the
        // typed one, which is then no defect.
        let headers = match path.pet_id.as_str() {
            "unsendable" | "overridden" => {
                let mut raw = axum::http::HeaderMap::new();
                raw.insert("x-trace", axum::http::HeaderValue::from_static("kept?"));
                if path.pet_id == "overridden" {
                    raw.insert("x-pet-note", axum::http::HeaderValue::from_static("by hand"));
                }
                server::ShowPetByIdOkHeaders {
                    x_pet_note: Some("first line\nsecond line".to_owned()),
                    raw,
                }
            }
            "padded" => server::ShowPetByIdOkHeaders {
                x_pet_note: Some(" padded ".to_owned()),
                ..Default::default()
            },
            _ => Default::default(),
        };
        server::ShowPetByIdResponse::Ok {
            headers,
            body: Box::new(types::read::Pet::from(types::Pet {
                id: 7,
                name: path.pet_id,
                tag: None,
            })),
        }
    }

    async fn upload_pet_photo(
        &self,
        _context: (),
        path: server::UploadPetPhotoPath,
        body: types::UploadPetPhotoBody,
    ) -> server::UploadPetPhotoResponse {
        self.record(|seen| {
            seen.photo_note = body.note.clone();
            seen.photo_file = Some(body.file.clone());
        });
        // A type only a response yields has no strict form; its read form is written out, and
        // `Default` fills whatever the answer leaves unsaid.
        server::UploadPetPhotoResponse::Created {
            headers: Default::default(),
            body: Box::new(types::read::PhotoUpload {
                pet_id: Some(path.pet_id),
                filename: Some("photo".to_owned()),
                content_type: Some("image/png".to_owned()),
                file_size: Some(body.file.len() as i64),
                note: body.note,
                rating: body.rating,
                ..Default::default()
            }),
        }
    }

    async fn typed_errors(
        &self,
        _context: (),
        path: server::TypedErrorsPath,
    ) -> server::TypedErrorsResponse {
        match path.kind.as_str() {
            "missing" => server::TypedErrorsResponse::NotFound {
                headers: Default::default(),
                body: Box::new(types::read::Error {
                    code: Some(404),
                    message: Some("missing".to_owned()),
                    ..Default::default()
                }),
            },
            "conflict" => server::TypedErrorsResponse::Conflict {
                headers: Default::default(),
                body: Box::new(types::read::Error {
                    code: Some(409),
                    message: Some("conflict".to_owned()),
                    ..Default::default()
                }),
            },
            // A reply whose declared headers are all it says, one of them text a reader must
            // not mistake for the boolean it spells.
            _ => server::TypedErrorsResponse::NoContent {
                headers: server::TypedErrorsNoContentHeaders {
                    x_outcome: Some("none".to_owned()),
                    x_flag: Some("true".to_owned()),
                    ..Default::default()
                },
            },
        }
    }

    async fn named_arms(
        &self,
        _context: (),
        path: server::NamedArmsPath,
    ) -> server::NamedArmsResponse {
        // The refusal's header group is numbered apart from `namedArmsBad`'s request builder.
        if path.kind == "refused" {
            return server::NamedArmsResponse::BadRequest {
                headers: server::NamedArmsBadRequest2Headers {
                    x_reason: Some("named apart".to_owned()),
                    ..Default::default()
                },
            };
        }
        server::NamedArmsResponse::NoContent {
            headers: Default::default(),
        }
    }

    async fn named_arms_bad(&self, _context: ()) -> server::NamedArmsBadResponse {
        server::NamedArmsBadResponse::NoContent {
            headers: Default::default(),
        }
    }

    async fn download_bytes(&self, _context: ()) -> server::DownloadBytesResponse {
        server::DownloadBytesResponse::Ok {
            headers: Default::default(),
            body: Box::new(vec![0, 255, b'{', b'\n']),
        }
    }

    async fn download_text(&self, _context: ()) -> server::DownloadTextResponse {
        server::DownloadTextResponse::Ok {
            headers: Default::default(),
            body: Box::new("plain text\nthat is not JSON".to_owned()),
        }
    }

    async fn rejection_probe(
        &self,
        _context: (),
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
        server::RejectionProbeResponse::NoContent {
            headers: Default::default(),
        }
    }
}
"#};

/// A second `Api` implementation, with a request context of its own.
///
/// The double asks nothing of the request; this one needs a caller, as an authenticated service
/// would, so the ordering between the context and generated decoding is driven over the wire.
const GATEKEEPER: &str = indoc::indoc! {r#"
/// Who is calling, as the middleware in front of the router left it on the request.
#[derive(Debug, Clone)]
struct Caller(String);

/// The double's operations, answered only for the one caller it admits.
#[derive(Debug, Clone)]
struct Gatekeeper {
    inner: Double,
    admitted: &'static str,
}

/// Read from the extension the middleware inserted, and checked against the state the router
/// hands the extractor, which is the implementation itself.
impl axum::extract::FromRequestParts<Gatekeeper> for Caller {
    type Rejection = (axum::http::StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        gatekeeper: &Gatekeeper,
    ) -> Result<Self, Self::Rejection> {
        let caller = parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or((axum::http::StatusCode::UNAUTHORIZED, "no caller"))?;
        if caller.0 != gatekeeper.admitted {
            return Err((axum::http::StatusCode::FORBIDDEN, "not admitted"));
        }
        Ok(caller)
    }
}

impl server::Api for Gatekeeper {
    type Context = Caller;

    async fn list_pets(&self, _: Caller, query: server::ListPetsQuery) -> server::ListPetsResponse {
        self.inner.list_pets((), query).await
    }

    async fn create_pets(&self, _: Caller) -> server::CreatePetsResponse {
        self.inner.create_pets(()).await
    }

    async fn list_pets_paged(
        &self,
        _: Caller,
        query: server::ListPetsPagedQuery,
    ) -> server::ListPetsPagedResponse {
        self.inner.list_pets_paged((), query).await
    }

    async fn show_pet_by_id(
        &self,
        _: Caller,
        path: server::ShowPetByIdPath,
    ) -> server::ShowPetByIdResponse {
        self.inner.show_pet_by_id((), path).await
    }

    async fn upload_pet_photo(
        &self,
        _: Caller,
        path: server::UploadPetPhotoPath,
        body: types::UploadPetPhotoBody,
    ) -> server::UploadPetPhotoResponse {
        self.inner.upload_pet_photo((), path, body).await
    }

    async fn typed_errors(
        &self,
        _: Caller,
        path: server::TypedErrorsPath,
    ) -> server::TypedErrorsResponse {
        self.inner.typed_errors((), path).await
    }

    async fn named_arms(
        &self,
        _: Caller,
        path: server::NamedArmsPath,
    ) -> server::NamedArmsResponse {
        self.inner.named_arms((), path).await
    }

    async fn named_arms_bad(&self, _: Caller) -> server::NamedArmsBadResponse {
        self.inner.named_arms_bad(()).await
    }

    async fn download_bytes(&self, _: Caller) -> server::DownloadBytesResponse {
        self.inner.download_bytes(()).await
    }

    async fn download_text(&self, _: Caller) -> server::DownloadTextResponse {
        self.inner.download_text(()).await
    }

    async fn rejection_probe(
        &self,
        caller: Caller,
        path: server::RejectionProbePath,
        query: server::RejectionProbeQuery,
        header: server::RejectionProbeHeader,
        cookie: server::RejectionProbeCookie,
        body: types::Pet,
    ) -> server::RejectionProbeResponse {
        self.inner.record(|seen| seen.probe_caller = Some(caller.0));
        self.inner
            .rejection_probe((), path, query, header, cookie, body)
            .await
    }

    /// A problem document that names the caller, which only the context knows.
    fn on_rejection(&self, caller: &Caller, rejection: server::Rejection) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let status = axum::http::StatusCode::from_u16(rejection.status())
            .unwrap_or(axum::http::StatusCode::BAD_REQUEST);
        (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/problem+json")],
            axum::Json(serde_json::json!({
                "title": rejection.to_string(),
                "caller": caller.0,
            })),
        )
            .into_response()
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
            })
            // A vendor that drifted on its headers: the required `Location` left out, the
            // `X-Legacy-Id` the description declares as one value sent twice, and the
            // `X-Pet-Tags` list split across two fields, which HTTP allows.
            .post(|| async {
                let mut headers = axum::http::HeaderMap::new();
                for (name, value) in [
                    ("retry-after", "30"),
                    ("x-legacy-id", "1"),
                    ("x-legacy-id", "2"),
                    ("x-pet-tags", "new, dog"),
                    ("x-pet-tags", "cat"),
                ] {
                    headers.append(name, axum::http::HeaderValue::from_static(value));
                }
                (axum::http::StatusCode::CREATED, headers)
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
        // declared length, `drifted` with drift in both its body and its declared header,
        // `failing` with the `default` arm and its header, and anything else with a root that is
        // not what the description declares at all.
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
                        // A required member sent as `null`, and the one `X-Pet-Note` the `200`
                        // declares sent twice.
                        "drifted" => {
                            let mut headers = axum::http::HeaderMap::new();
                            for note in ["first", "second"] {
                                headers.append("x-pet-note", axum::http::HeaderValue::from_static(note));
                            }
                            (headers, axum::Json(serde_json::json!({"id": 7, "name": null})))
                                .into_response()
                        }
                        "failing" => (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            [("x-request-id", "r-9")],
                            axum::Json(serde_json::json!({"code": 500, "message": "down"})),
                        )
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
    assert_eq!(pet.into_value().body.name.as_deref(), Some("a pet/with slashes"));
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
    assert_eq!(name.member(), "name");
    let kinds: Vec<types::DegradationKind> =
        report.at(name.site()).map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [types::DegradationKind::RequiredAbsent, types::DegradationKind::NullNotAllowed]
    );
    // The member's entries belong to the type; the type's own site holds only what the type
    // itself tolerated, which here is the member the description never declared.
    assert!(report.within(types::read::Pet::SITE).count() > kinds.len());
    assert!(!report.touches(types::read::Pet::SITE_TAG.site()));
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

/// Start the gatekeeper behind a middleware that turns an `x-caller` header into the request
/// extension its context reads, as an authenticating layer would.
async fn gated_serving() -> eyre::Result<(Double, client::Client)> {
    let double = Double::default();
    let gatekeeper = Gatekeeper {
        inner: double.clone(),
        admitted: "alice",
    };
    let router = server::router(gatekeeper).layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            let caller = request
                .headers()
                .get("x-caller")
                .and_then(|value| value.to_str().ok())
                .map(|name| Caller(name.to_owned()));
            if let Some(caller) = caller {
                request.extensions_mut().insert(caller);
            }
            next.run(request).await
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((double, client::Client::new(format!("http://{address}"))))
}

/// A well-formed `rejectionProbe` request.
fn probe_params() -> client::RejectionProbeParams {
    client::RejectionProbeParams {
        id: 9,
        limit: 11,
        x_mode: "strict".to_owned(),
        session: 1234,
        body: types::Pet { id: 1, name: "p".to_owned(), tag: None },
    }
}

/// A context reads what a middleware left on the request, and the operation receives it.
#[test_util::test]
async fn a_request_context_reaches_the_operation() {
    let caller = reqwest::header::HeaderName::from_static("x-caller");
    let (double, client) = gated_serving().await?;
    client
        .rejection_probe(probe_params())
        .header(caller.clone(), reqwest::header::HeaderValue::from_static("alice"))
        .send()
        .await?;
    assert_eq!(double.seen().probe_caller.as_deref(), Some("alice"));

    // The extractor is handed the implementation as its state, and refuses a caller it does not
    // admit before the operation runs.
    let (double, client) = gated_serving().await?;
    let error = client
        .rejection_probe(probe_params())
        .header(caller, reqwest::header::HeaderValue::from_static("mallory"))
        .send()
        .await
        .err()
        .ok_or_eyre("a caller the state does not admit is refused")?;
    assert_eq!(error.status().map(|status| status.as_u16()), Some(403));
    assert_eq!(double.seen(), Seen::default());
}

/// The context is extracted before anything is decoded, so a malformed request without a caller
/// is refused by the context, and `on_rejection` never sees it.
#[test_util::test]
async fn a_failing_context_wins_over_a_malformed_request() {
    let (_, client) = gated_serving().await?;
    // No `session` cookie, which the description requires: a `400` once decoding runs.
    let response = reqwest::Client::new()
        .post(format!("{}/rejection-probe/9?limit=11", client.base_url()))
        .header("x-mode", "strict")
        .json(&types::Pet { id: 1, name: "p".to_owned(), tag: None })
        .send()
        .await?;
    assert_eq!(response.status(), 401);
    assert_eq!(response.text().await?, "no caller");
}

/// A request the context admits and decoding then rejects reaches `on_rejection` with that
/// context, which an override can put in its reply.
#[test_util::test]
async fn a_generated_rejection_sees_the_context() {
    let (double, client) = gated_serving().await?;
    let response = reqwest::Client::new()
        .post(format!("{}/rejection-probe/9?limit=11", client.base_url()))
        .header("x-caller", "alice")
        .header("x-mode", "strict")
        .json(&types::Pet { id: 1, name: "p".to_owned(), tag: None })
        .send()
        .await?;
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.headers().get(reqwest::header::CONTENT_TYPE),
        Some(&reqwest::header::HeaderValue::from_static("application/problem+json"))
    );
    let body: serde_json::Value = response.json().await?;
    assert_eq!(body["caller"], "alice", "{body}");
    let title = body["title"].as_str().unwrap_or_default();
    assert!(title.contains("session"), "{body}");
    assert_eq!(double.seen(), Seen::default());
}

/// A `default` answered with a status another arm claims is the handler's defect, refused `500`
/// rather than sent as a body a client would decode as that arm's, and without the headers the
/// handler gave the `default`.
#[test_util::test]
async fn a_default_answered_with_a_claimed_status_is_refused() {
    let (_, client) = serving().await?;
    let response = reqwest::Client::new()
        .get(format!("{}/pets/misrouted", client.base_url()))
        .send()
        .await?;
    assert_eq!(response.status(), 500);
    assert_eq!(response.headers().get("x-request-id"), None);
    let text = response.text().await?;
    assert!(text.contains("`default` with 200"), "{text}");
}

/// Every value of one header a response carries, in order.
fn values(headers: &reqwest::header::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .collect()
}

/// A typed header reaches the wire as its `simple` spelling, a scalar whole and a list in one
/// field, and `raw` replaces the typed field of the same name with every value it holds.
/// Read off the response raw, which is what the server wrote whatever the client makes of it.
#[test_util::test]
async fn declared_response_headers_reach_the_wire() {
    let (_, client) = serving().await?;
    let created = client.create_pets().send().await?;
    assert_eq!(created.status(), 201);
    let headers = created.headers();

    // The typed fields, under the names the description spells
    assert_eq!(values(headers, "location"), ["/pets/7"]);
    assert_eq!(values(headers, "x-pet-tags"), ["new,dog"]);
    assert_eq!(values(headers, "x-legacy-id"), ["7"]);

    // The raw headers: one replacing the typed `Retry-After`, one repeated in order
    assert_eq!(values(headers, "retry-after"), ["soon"]);
    assert_eq!(values(headers, "x-trace"), ["first", "second"]);
}

/// A `204` that declares a header and no content carries the header alone.
#[test_util::test]
async fn a_header_only_reply_carries_its_declared_header() {
    let (_, client) = serving().await?;
    let reply = client
        .typed_errors(client::TypedErrorsParams { kind: "neither".to_owned() })
        .send()
        .await?;
    assert_eq!(reply.status(), 204);
    assert_eq!(values(reply.headers(), "x-outcome"), ["none"]);
    // And the client reads it into its typed field, with no body to read it beside.
    assert_eq!(reply.value().headers.x_outcome.as_deref(), Some("none"));
    // A header its schema says nothing about is the text it was written as on both sides,
    // never the boolean `true` spells.
    assert_eq!(values(reply.headers(), "x-flag"), ["true"]);
    assert_eq!(reply.value().headers.x_flag.as_deref(), Some("true"));
    assert!(!reply.is_degraded(), "{}", reply.degradations());
}

/// An arm whose payload would be named like another operation's request builder is numbered
/// apart from it, with its header group beside it, and both operations still answer.
#[test_util::test]
async fn an_arm_type_named_like_another_operations_type_is_numbered_apart() {
    let (_, client) = serving().await?;
    let error = client
        .named_arms(client::NamedArmsParams { kind: "refused".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("the `400` is a declared failure")?;
    let client::Error::Declared(response) = &error else {
        eyre::bail!("expected a declared error, got {error:?}");
    };
    assert_eq!(response.status(), 400);
    let refusal: &client::NamedArmsBadRequest2 = response.value();
    assert_eq!(refusal.headers.x_reason.as_deref(), Some("named apart"));

    let request: client::NamedArmsBadRequest<'_> = client.named_arms_bad();
    assert_eq!(request.send().await?.status(), 204);
}

/// The headers a response declares arrive typed, in one payload with its body, while every
/// header stays on the response raw.
/// A raw override the declaration cannot read is `None` and reported, rather than a call that
/// fails over one header.
#[test_util::test]
async fn declared_response_headers_decode_into_typed_fields() {
    let (_, client) = serving().await?;
    let created = client.create_pets().send().await?;
    let headers = &created.value().headers;
    assert_eq!(headers.location.as_deref(), Some("/pets/7"));
    assert_eq!(headers.x_pet_tags, Some(vec!["new".to_owned(), "dog".to_owned()]));
    #[expect(deprecated, reason = "the test reads the header the description deprecates")]
    let legacy = headers.x_legacy_id;
    assert_eq!(legacy, Some(7));

    // `Retry-After: soon`, which the double's raw headers put over its typed thirty seconds, is
    // no number of seconds: the field is `None`, the report says why, and the raw value stays.
    assert_eq!(headers.retry_after, None);
    let report = created.degradations();
    assert_eq!(report.len(), 1, "{report}");
    let entry = report.iter().next().ok_or_eyre("the malformed header is reported")?;
    assert_eq!(entry.kind, types::DegradationKind::Undecodable);
    assert_eq!(entry.site.type_name(), "CreatePetsCreatedHeaders");
    assert_eq!(
        entry.site.pointer(),
        "/paths/~1pets/post/responses/201/headers/Retry-After"
    );
    assert!(entry.samples.iter().any(|sample| sample.contains("soon")), "{report}");
    assert_eq!(values(created.headers(), "retry-after"), ["soon"]);

    // A JSON body and its declared header are one payload; the double's `200` leaves out the
    // optional `X-Pet-Note`, which is no drift.
    let pet = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "rex".to_owned() })
        .send()
        .await?;
    assert_eq!(pet.value().body.name.as_deref(), Some("rex"));
    assert_eq!(pet.value().headers.x_pet_note, None);
    assert!(!pet.is_degraded(), "{}", pet.degradations());
}

/// A vendor that drifted on its headers: a required one left out is reported, one declared as a
/// single value that arrives twice is refused rather than picked, and a list split across two
/// fields is read whole.
#[test_util::test]
async fn header_drift_is_tolerated_and_reported_header_by_header() {
    let client = raw_serving().await?;
    let created = client.create_pets().send().await?;
    let headers = &created.value().headers;
    assert_eq!(headers.location, None);
    assert_eq!(headers.retry_after, Some(30));
    assert_eq!(
        headers.x_pet_tags,
        Some(vec!["new".to_owned(), "dog".to_owned(), "cat".to_owned()])
    );
    #[expect(deprecated, reason = "the test reads the header the description deprecates")]
    let legacy = headers.x_legacy_id;
    assert_eq!(legacy, None);

    // One entry per header that drifted, keyed at the place the description declares it.
    let report = created.degradations();
    let kinds_at = |header: &str| -> Vec<types::DegradationKind> {
        let at = format!("/paths/~1pets/post/responses/201/headers/{header}");
        report
            .iter()
            .filter(|entry| entry.site.pointer() == at)
            .map(|entry| entry.kind)
            .collect()
    };
    assert_eq!(kinds_at("Location"), [types::DegradationKind::RequiredAbsent]);
    assert_eq!(kinds_at("X-Legacy-Id"), [types::DegradationKind::Undecodable]);
    assert_eq!(report.len(), 2, "{report}");
    assert!(report.to_string().contains("2 fields"), "{report}");
    // Both values are still on the response, as they arrived.
    assert_eq!(values(created.headers(), "x-legacy-id"), ["1", "2"]);
}

/// A response whose body and declared header both drifted reaches the observer once, with one
/// report holding both, which is the report the response carries.
/// A declared error carries its typed headers beside its body too.
#[test_util::test]
async fn header_and_body_drift_reach_the_observer_as_one_report() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let client = raw_serving().await?.observe({
        let seen = Arc::clone(&seen);
        move |degraded: client::Degraded<'_>| {
            if let Ok(mut seen) = seen.lock() {
                seen.push(degraded.degradations.to_string());
            }
        }
    });
    let pet = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "drifted".to_owned() })
        .send()
        .await?;
    let report = pet.degradations();
    assert!(report.touches(types::read::Pet::SITE_NAME.site()), "{report}");
    assert!(
        report.iter().any(|entry| entry.site.type_name() == "ShowPetByIdOkHeaders"
            && entry.kind == types::DegradationKind::Undecodable),
        "{report}"
    );
    assert_eq!(pet.value().body.id, Some(7));
    assert_eq!(pet.value().headers.x_pet_note, None);
    let observed = seen.lock().map_err(|_| eyre::eyre!("poisoned"))?.clone();
    assert_eq!(observed, [report.to_string()]);

    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "failing".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("the `default` arm is a declared failure")?;
    let client::Error::Declared(response) = &error else {
        eyre::bail!("expected a declared error, got {error:?}");
    };
    assert_eq!(response.status(), 500);
    assert_eq!(response.value().headers.x_request_id.as_deref(), Some("r-9"));
    assert_eq!(response.value().body.message.as_deref(), Some("down"));
}

/// A typed header value no HTTP field can carry is the handler's defect: a `500` that carries
/// none of the arm's headers, typed or raw, rather than a reply with the header dropped.
#[test_util::test]
async fn a_header_the_handler_cannot_send_answers_without_the_arm_headers() {
    let (_, client) = serving().await?;
    let response = reqwest::Client::new()
        .get(format!("{}/pets/unsendable", client.base_url()))
        .send()
        .await?;
    assert_eq!(response.status(), 500);
    assert_eq!(values(response.headers(), "x-pet-note"), [] as [String; 0]);
    assert_eq!(values(response.headers(), "x-trace"), [] as [String; 0]);
    let text = response.text().await?;
    assert!(text.contains("`X-Pet-Note`"), "{text}");

    // Whitespace around a value, which HTTP strips, is refused the same way.
    let response = reqwest::Client::new()
        .get(format!("{}/pets/padded", client.base_url()))
        .send()
        .await?;
    assert_eq!(response.status(), 500);
    let text = response.text().await?;
    assert!(text.contains("whitespace"), "{text}");
}

/// A raw value replaces the typed one of its name before the typed one is checked, so a handler
/// can send by hand what its typed field cannot hold, and the client reads the raw value.
#[test_util::test]
async fn a_raw_header_overrides_a_typed_one_that_cannot_be_sent() {
    let (_, client) = serving().await?;
    let pet = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "overridden".to_owned() })
        .send()
        .await?;
    assert_eq!(pet.status(), 200);
    assert_eq!(values(pet.headers(), "x-pet-note"), ["by hand"]);
    assert_eq!(values(pet.headers(), "x-trace"), ["kept?"]);
    assert_eq!(pet.value().headers.x_pet_note.as_deref(), Some("by hand"));
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
        Ok((pet.into_value().body.name.unwrap_or_default(), listed))
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
    assert_eq!(pet.into_value().body.name.as_deref(), Some("key=explicit"));

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

/// A server that answers every request with `status` and a body that breaks off: the headers
/// promise more bytes than follow, and the connection then closes.
/// It also sets a cookie, as a header that carries a secret.
///
/// Written on a raw socket rather than through axum, because hyper may refuse a body stream
/// that fails at once before it has sent the status, and the point is a failure after it.
/// The bytes that do arrive are a whole JSON value, so only the framing can fail the read.
async fn truncating_serving(status: u16, body: &'static str) -> eyre::Result<client::Client> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // The request is read up to the end of its headers; none of these carry a body.
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let promised = body.len() + 64;
                let head = indoc::formatdoc! {"
                    HTTP/1.1 {status} Answered
                    content-type: application/json
                    set-cookie: session=cookie-secret
                    content-length: {promised}

                "}
                .replace('\n', "\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Ok(client::Client::new(format!("http://{address}")))
}

/// A body that breaks off after its status is a failed read carrying that status, not a body
/// that failed to decode: a caller classifies it as the transport failure it is.
#[test_util::test]
async fn a_body_that_breaks_off_is_a_failed_read_rather_than_a_bad_payload() {
    // Read whole, with a query key the error must not carry.
    let client = truncating_serving(200, "{\"id\": 7, \"name\": \"Rex\"}")
        .await?
        .with_query_key("s3cret")
        .with_cookie_key("c");
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "7".to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("a body that breaks off is refused")?;
    let client::Error::BodyRead(read) = &error else {
        eyre::bail!("expected BodyRead, got {error:?}");
    };
    assert_eq!(read.status().as_u16(), 200);
    assert_eq!(error.status().map(|status| status.as_u16()), Some(200));
    // Not every `reqwest` names the URL on a failed read; where it does, the key is not in it.
    if let Some(url) = read.reqwest_error().url() {
        assert_eq!(url.query(), None, "{url}");
    }
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains("s3cret"), "{rendered}");
    }

    // Read in chunks under a limit the promised length does not pass, so the limit is not
    // what refuses it.
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: "7".to_owned() })
        .response_body_limit(1 << 20)
        .send()
        .await
        .err()
        .ok_or_eyre("a body that breaks off under a limit is refused")?;
    assert!(matches!(error, client::Error::BodyRead(_)), "{error:?}");

    // A declared error status reads its body the same way, and keeps its own status.
    let client = truncating_serving(409, "{\"code\": 409, \"message\": \"conflict\"}").await?;
    let error = client
        .typed_errors(client::TypedErrorsParams {
            kind: "conflict".to_owned(),
        })
        .send()
        .await
        .err()
        .ok_or_eyre("a declared failure whose body breaks off is refused")?;
    let client::Error::BodyRead(read) = &error else {
        eyre::bail!("expected BodyRead, got {error:?}");
    };
    assert_eq!(read.status().as_u16(), 409);
}

/// Asserts that none of `secrets` reaches the error's `Display` or `Debug` output.
fn assert_hidden(error: &client::Error<impl std::fmt::Debug>, secrets: &[&str]) {
    for rendered in [error.to_string(), format!("{error:?}")] {
        for secret in secrets {
            assert!(!rendered.contains(secret), "`{secret}` in {rendered}");
        }
    }
}

/// No error names the URL it requested: a path segment can be a credential — an access token
/// read back by its own value — and so can any other part of a URL.
#[test_util::test]
async fn no_error_names_the_url_it_requested() {
    let secret = "secret-in-the-path";

    // Both ways a request is sent, through the credentials and without them, hide a secret in
    // the path the operation adds and in the base URL's own when the request never completes.
    // The connection error underneath still names the address it dialed, which is not the URL.
    let client = client::Client::new(format!("http://127.0.0.1:9/{secret}-base"));
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: secret.to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("nothing answers on the discard port")?;
    assert!(matches!(error, client::Error::Request(_)), "{error:?}");
    assert_hidden(&error, &[secret]);
    let error = client
        .list_pets(client::ListPetsParams { limit: None })
        .send()
        .await
        .err()
        .ok_or_eyre("nothing answers on the discard port")?;
    assert!(matches!(error, client::Error::Request(_)), "{error:?}");
    assert_hidden(&error, &[secret]);

    // A body that breaks off after its status.
    let client = truncating_serving(200, "{\"id\": 7, \"name\": \"Rex\"}").await?;
    let error = client
        .show_pet_by_id(client::ShowPetByIdParams { pet_id: secret.to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("a body that breaks off is refused")?;
    assert!(matches!(error, client::Error::BodyRead(_)), "{error:?}");
    assert_hidden(&error, &[secret]);

    // A status the description does not declare, handed back with its response: the URL stays
    // on the response for a caller who asks for it, and only there.
    let client = truncating_serving(418, "").await?;
    let error = client
        .typed_errors(client::TypedErrorsParams { kind: secret.to_owned() })
        .send()
        .await
        .err()
        .ok_or_eyre("an undeclared status is refused")?;
    let client::Error::UnexpectedStatus(response) = &error else {
        eyre::bail!("expected UnexpectedStatus, got {error:?}");
    };
    assert!(response.url().path().contains(secret), "{}", response.url());
    assert!(response.headers().contains_key("set-cookie"));
    assert_hidden(&error, &[secret, "cookie-secret"]);
}
"#};
