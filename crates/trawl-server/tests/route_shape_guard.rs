// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A source guard for ADR-0059's rule that an authenticated route judges
//! authority before request shape.
//!
//! An extractor that can refuse a request, such as `Path<_>`, `Query<_>`,
//! `Json<_>` or `Bytes` bound bare, lets axum answer its own 400, 413 or
//! 422 before the handler runs, so a key without the route's permission
//! learns whether its request parsed or how large its body was. Every
//! handler takes those as `Result<_, …Rejection>`, checks its authority,
//! and only then maps the rejection through `shape::shaped`.
//!
//! This test reads the router in `transport/http.rs`, resolves every
//! handler its `.route(…)` registrations name to the source file that
//! defines it, and fails when a registered handler lives in a file the
//! guard does not read. In each registered handler it fails, naming the
//! file, the function and the parameter, on any parameter whose type is
//! not on [`NON_REJECTING`] and is not a `Result<_, …Rejection>`: the scan
//! fails closed, so a type alias or an extractor it does not know must be
//! named here to pass. A tuple parameter is judged by each of its types,
//! a `use … as` alias is resolved within its file, and a turbofish path
//! names the same type. The behavioral check of each route is
//! `every_handlers_route_judges_authority_before_request_shape` in
//! `http_api.rs` and its siblings in `investigations_api.rs`; this one keeps
//! a new route from skipping that table without anyone noticing.
//!
//! Handlers on the unauthenticated router are exempt by name. The list is
//! checked against `transport/http.rs` so it cannot rot: each name must
//! still be a function in the sources and still be mounted outside the
//! `authenticated` router. It needs no database.

use std::fs;
use std::path::Path;

/// The sources that hold authenticated handlers.
const SOURCES: &[&str] = &[
    "src/handlers.rs",
    "src/investigations/routes.rs",
    "src/results/routes.rs",
    "src/ingest/preview.rs",
    "src/ingest/handler.rs",
];

/// Handlers mounted outside the `authenticated` router in
/// `transport/http.rs` (`/api/v1/health`, `/metrics`).
const UNAUTHENTICATED: &[&str] = &["health", "prometheus_metrics"];

/// Extractors that never refuse a request on what the caller sent, by the
/// last segment of their type's path: the router's state, extensions the
/// server's own layers insert, the headers, the raw query string, the raw
/// request and body, and the ingest route's key, whose refusal is the
/// route's authority check.
const NON_REJECTING: &[&str] = &[
    "State",
    "Extension",
    "HeaderMap",
    "RawQuery",
    "Request",
    "Body",
    "IngestKey",
];

/// The method routers a `.route(…)` registration names its handlers with.
const METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace", "any",
];

/// Drop `//` comments, so prose that mentions `async fn` is not a function.
fn without_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| line.find("//").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split `text` at commas that are not inside `()`, `<>`, `[]` or `{}`.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    let mut previous = ' ';
    for (at, c) in text.char_indices() {
        match c {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            // `->` is not a closing angle bracket.
            '>' if previous != '-' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..at]);
                start = at + 1;
            }
            _ => {}
        }
        previous = c;
    }
    parts.push(&text[start..]);
    parts
}

/// A parameter as (pattern, type): split at the first `:` outside
/// parentheses and not half of a `::`.
fn split_parameter(param: &str) -> Option<(&str, &str)> {
    let bytes = param.as_bytes();
    let mut depth = 0_i32;
    for (at, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b':' if depth == 0 => {
                if bytes.get(at + 1) == Some(&b':') || (at > 0 && bytes[at - 1] == b':') {
                    continue;
                }
                return Some((&param[..at], &param[at + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// The text between the `(` at `open` in `text` and its matching `)`.
fn parenthesized(text: &str, open: usize) -> Option<&str> {
    let mut depth = 0_i32;
    for (at, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open + 1..open + at]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The last path segment before the first `<` of `ty`: `axum::Json<T>`
/// and the turbofish `Json::<T>` are `Json`. A type with no `<` is
/// returned whole.
fn outer_name(ty: &str) -> &str {
    let head = ty.split('<').next().unwrap_or(ty).trim();
    let head = head.strip_suffix("::").unwrap_or(head);
    head.rsplit("::").next().unwrap_or(head).trim()
}

/// The text between the first `<` of `ty` and its matching `>`.
fn generic_args(ty: &str) -> &str {
    let Some(open) = ty.find('<') else { return "" };
    let close = ty.rfind('>').unwrap_or(ty.len());
    &ty[open + 1..close]
}

/// The names `source` imports under another name with `use … as`, as
/// (alias, the imported item's last path segment).
fn use_aliases(source: &str) -> Vec<(String, String)> {
    let source = without_line_comments(source);
    let mut aliases = Vec::new();
    let mut rest = source.as_str();
    while let Some(at) = rest.find("use ") {
        let starts_word =
            at == 0 || !rest[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
        rest = &rest[at + "use ".len()..];
        if !starts_word {
            continue;
        }
        let Some(end) = rest.find(';') else { break };
        let flattened = rest[..end].replace(['{', '}'], ",");
        for item in flattened.split(',') {
            let words: Vec<&str> = item.split_whitespace().collect();
            if let [path, "as", alias] = words.as_slice() {
                let name = path.rsplit("::").next().unwrap_or(path);
                aliases.push(((*alias).to_owned(), name.to_owned()));
            }
        }
        rest = &rest[end..];
    }
    aliases
}

/// `name` with a `use … as` alias of its file resolved.
fn resolved<'a>(name: &'a str, aliases: &'a [(String, String)]) -> &'a str {
    aliases
        .iter()
        .find(|(alias, _)| alias == name)
        .map_or(name, |(_, original)| original.as_str())
}

/// Whether a parameter typed `ty` can refuse the request before the
/// handler runs: a tuple if any of its types can; a `Result` unless its
/// error is an axum `…Rejection`; anything else unless it is on
/// [`NON_REJECTING`].
fn refuses_before_the_handler(ty: &str, aliases: &[(String, String)]) -> bool {
    let ty = ty.trim();
    if let Some(inner) = ty.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        return split_top_level(inner)
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .any(|part| refuses_before_the_handler(part, aliases));
    }
    let name = resolved(outer_name(ty), aliases);
    if NON_REJECTING.contains(&name) {
        return false;
    }
    if name != "Result" {
        return true;
    }
    match split_top_level(generic_args(ty)).as_slice() {
        [_, err] => !resolved(outer_name(err.trim()), aliases).ends_with("Rejection"),
        _ => true,
    }
}

/// Every `async fn` in `source`, as (name, parameter list).
fn async_fns(source: &str) -> Vec<(String, String)> {
    let source = without_line_comments(source);
    let mut found = Vec::new();
    let mut rest = source.as_str();
    while let Some(at) = rest.find("async fn ") {
        rest = &rest[at + "async fn ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let Some(open) = rest.find('(') else { break };
        let Some(params) = parenthesized(rest, open) else {
            break;
        };
        found.push((name, params.to_owned()));
        rest = &rest[open + 1 + params.len()..];
    }
    found
}

/// One line per offending parameter of the handlers `handlers` names in
/// `source`, which `file` names. Every `async fn` of that name is judged.
fn offences_in(file: &str, source: &str, handlers: &[&str]) -> Vec<String> {
    let aliases = use_aliases(source);
    let mut offences = Vec::new();
    for (name, params) in async_fns(source) {
        if !handlers.contains(&name.as_str()) {
            continue;
        }
        for param in split_top_level(&params) {
            let Some((pattern, ty)) = split_parameter(param) else {
                continue;
            };
            if refuses_before_the_handler(ty, &aliases) {
                offences.push(format!(
                    "{file}: {name}: parameter `{}` is typed `{}`",
                    pattern.trim(),
                    ty.trim()
                ));
            }
        }
    }
    offences
}

/// [`offences_in`] for every `async fn` of `source`, as for a fixture.
fn offences_of_every_fn(file: &str, source: &str) -> Vec<String> {
    let fns = async_fns(source);
    let names: Vec<&str> = fns.iter().map(|(name, _)| name.as_str()).collect();
    offences_in(file, source, &names)
}

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The body of `router` in `transport/http.rs`, comments dropped.
fn router_body(http: &str) -> String {
    let start = http
        .find("pub fn router(")
        .expect("transport/http.rs no longer defines `pub fn router(`");
    let end = http[start..]
        .find("\n}\n")
        .map(|at| start + at)
        .expect("`router` has no closing brace at the start of a line");
    without_line_comments(&http[start..end])
}

/// The handler paths the `.route(…)` registrations of `router` name, such
/// as `handlers::query` or `ingest::preview::preview`, each once.
fn registered_handlers(router: &str) -> Vec<String> {
    let mut handlers: Vec<String> = Vec::new();
    let mut rest = router;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route".len()..];
        let args = parenthesized(rest, 0).expect("a `.route(` without its `)`");
        let parts: Vec<&str> = split_top_level(args)
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect();
        let [_, methods] = parts.as_slice() else {
            panic!("a `.route(` with other than two arguments: {args}");
        };
        for method in METHODS {
            let call = format!("{method}(");
            let mut scan = *methods;
            while let Some(found) = scan.find(&call) {
                let before = scan[..found].chars().next_back();
                let open = found + method.len();
                let inside = parenthesized(scan, open).expect("a method call without its `)`");
                scan = &scan[open + 1..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let handler = inside.trim();
                assert!(
                    !handler.is_empty()
                        && handler
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == ':'),
                    "the router registers a handler that is not a path: `{handler}`"
                );
                if !handlers.iter().any(|h| h == handler) {
                    handlers.push(handler.to_owned());
                }
            }
        }
    }
    handlers
}

/// The `use crate::…` imports of `http`, as (the name in scope, the path
/// under the crate root).
fn crate_imports(http: &str) -> Vec<(String, String)> {
    let mut imports = Vec::new();
    for line in without_line_comments(http).lines() {
        let Some(path) = line
            .trim()
            .strip_prefix("use crate::")
            .and_then(|l| l.strip_suffix(';'))
        else {
            continue;
        };
        let (prefix, items) = match path.split_once('{') {
            Some((prefix, items)) => (prefix, items.trim_end_matches('}').split(',').collect()),
            None => ("", vec![path]),
        };
        for item in items {
            let words: Vec<&str> = item.split_whitespace().collect();
            let (item_path, name) = match words.as_slice() {
                [item_path, "as", alias] => (*item_path, (*alias).to_owned()),
                [item_path] => (
                    *item_path,
                    item_path
                        .rsplit("::")
                        .next()
                        .unwrap_or(item_path)
                        .to_owned(),
                ),
                _ => continue,
            };
            imports.push((name, format!("{prefix}{item_path}")));
        }
    }
    imports
}

/// The source file, relative to the crate, that defines `handler`, a path
/// the router names, and the handler's function name.
fn handler_file(handler: &str, imports: &[(String, String)]) -> (String, String) {
    let segments: Vec<&str> = handler.split("::").collect();
    let [first, middle @ .., name] = segments.as_slice() else {
        panic!("the router registers `{handler}`, which names no module");
    };
    let (_, base) = imports
        .iter()
        .find(|(in_scope, _)| in_scope == first)
        .unwrap_or_else(|| {
            panic!(
                "transport/http.rs registers `{handler}` but imports no `{first}` from the crate"
            )
        });
    let mut module = base.replace("::", "/");
    for segment in middle {
        module = format!("{module}/{segment}");
    }
    let candidates = [format!("src/{module}.rs"), format!("src/{module}/mod.rs")];
    let file = candidates
        .iter()
        .find(|file| Path::new(env!("CARGO_MANIFEST_DIR")).join(file).is_file())
        .unwrap_or_else(|| panic!("`{handler}` resolves to none of {candidates:?}"));
    (file.clone(), (*name).to_owned())
}

/// Every authenticated handler the router registers, as (file, name).
fn authenticated_handlers() -> Vec<(String, String)> {
    let http = read("src/transport/http.rs");
    let imports = crate_imports(&http);
    let registered = registered_handlers(&router_body(&http));
    assert!(
        !registered.is_empty(),
        "no `.route(…)` registration found in `router`, so the scan would pass for nothing"
    );
    registered
        .iter()
        .map(|handler| handler_file(handler, &imports))
        .filter(|(_, name)| !UNAUTHENTICATED.contains(&name.as_str()))
        .collect()
}

/// Every handler the router registers lives in a file this guard reads,
/// and is an `async fn` there, so a route cannot leave the scan by living
/// somewhere new.
#[test]
fn every_registered_handler_is_scanned() {
    let handlers = authenticated_handlers();
    for (file, name) in &handlers {
        assert!(
            SOURCES.contains(&file.as_str()),
            "transport/http.rs registers `{name}` from {file}, which this guard does not \
             read: add it to SOURCES"
        );
        assert!(
            async_fns(&read(file)).iter().any(|(n, _)| n == name),
            "transport/http.rs registers `{name}` but {file} has no `async fn {name}`"
        );
    }
    for file in SOURCES {
        assert!(
            handlers.iter().any(|(f, _)| f == file),
            "{file} is in SOURCES but the router registers no handler from it"
        );
    }
}

/// No authenticated handler takes a parameter that can refuse the request
/// before its authority check.
#[test]
fn no_authenticated_handler_binds_a_bare_extractor() {
    let handlers = authenticated_handlers();
    let mut offences = Vec::new();
    for file in SOURCES {
        let names: Vec<&str> = handlers
            .iter()
            .filter(|(f, _)| f == file)
            .map(|(_, name)| name.as_str())
            .collect();
        offences.extend(offences_in(file, &read(file), &names));
    }
    assert!(
        offences.is_empty(),
        "{} handler parameter(s) take an extractor that can refuse before the \
         authority check (ADR-0059). Take it as `Result<_, …Rejection>`, check \
         authority, then pass it through `shape::shaped`; an extractor that never \
         refuses goes on NON_REJECTING:\n{}",
        offences.len(),
        offences.join("\n")
    );
}

/// Every exempt name is still a function in the sources and still mounted
/// outside the `authenticated` router, so the list cannot outlive its
/// handlers or hide a route that gained authentication.
#[test]
fn the_unauthenticated_list_matches_the_router() {
    let sources: Vec<(String, Vec<String>)> = SOURCES
        .iter()
        .map(|file| {
            let names = async_fns(&read(file)).into_iter().map(|(n, _)| n).collect();
            ((*file).to_owned(), names)
        })
        .collect();
    let http = read("src/transport/http.rs");
    let start = http
        .find("let app = Router::new()")
        .expect("transport/http.rs no longer builds `let app = Router::new()`");
    let end = http[start..]
        .find(".nest(\"/api/v1\", authenticated)")
        .map(|at| start + at)
        .expect("transport/http.rs no longer nests the authenticated router");
    let outside = &http[start..end];
    for name in UNAUTHENTICATED {
        assert!(
            sources
                .iter()
                .any(|(_, names)| names.iter().any(|n| n == name)),
            "`{name}` is on the unauthenticated list but no scanned file has `async fn {name}`"
        );
        assert!(
            outside.contains(&format!("handlers::{name}")),
            "`{name}` is on the unauthenticated list but transport/http.rs no longer \
             mounts it outside the authenticated router"
        );
    }
}

/// The router parser: method chains, nested module paths and comments.
#[test]
fn the_router_parser_finds_each_registration() {
    let router = without_line_comments(
        r#"
        let app = Router::new()
            .route("/a", post(handlers::a).get(handlers::b))
            .route(
                "/c/{id}",
                put(routes::c)
                    .delete(routes::delete),
            )
            // .route("/hidden", get(handlers::hidden))
            .route("/d", get(ingest::preview::d))
            .route_layer(middleware::from_fn(failure::mark_handler));
        "#,
    );
    assert_eq!(
        registered_handlers(&router),
        [
            "handlers::b",
            "handlers::a",
            "routes::c",
            "routes::delete",
            "ingest::preview::d"
        ]
    );
    let imports = crate_imports(
        "use crate::handlers;\nuse crate::results::routes;\n\
         use crate::policy::{normalize_auth_errors, require_trawl_grant};\n\
         use crate::investigations::routes as investigations;\n",
    );
    assert_eq!(
        imports,
        [
            ("handlers".to_owned(), "handlers".to_owned()),
            ("routes".to_owned(), "results::routes".to_owned()),
            (
                "normalize_auth_errors".to_owned(),
                "policy::normalize_auth_errors".to_owned()
            ),
            (
                "require_trawl_grant".to_owned(),
                "policy::require_trawl_grant".to_owned()
            ),
            (
                "investigations".to_owned(),
                "investigations::routes".to_owned()
            ),
        ]
    );
    assert_eq!(
        handler_file("investigations::list", &imports),
        ("src/investigations/routes.rs".to_owned(), "list".to_owned())
    );
}

/// Negative controls: each way past a name-matching scan is one offence.
/// Every fixture is valid Rust.
#[test]
fn the_scanner_flags_each_bypass_once() {
    let fixtures = [
        (
            "a tuple of extractors",
            r"
            use axum::extract::{Path, Query, State};
            pub async fn tuple(
                State(state): State<AppState>,
                (Path(id), Query(params)): (Path<String>, Query<Params>),
            ) -> StatusCode {
                StatusCode::OK
            }
            ",
            "fixture.rs: tuple: parameter `(Path(id), Query(params))` is typed \
             `(Path<String>, Query<Params>)`",
        ),
        (
            "an extractor imported under another name",
            r"
            use axum::extract::{Path as P, State};
            pub async fn aliased(State(state): State<AppState>, P(id): P<String>) {}
            ",
            "fixture.rs: aliased: parameter `P(id)` is typed `P<String>`",
        ),
        (
            "a type alias",
            r"
            use axum::extract::Query;
            type Params = Query<ReadParams>;
            pub async fn typed(params: Params) {}
            ",
            "fixture.rs: typed: parameter `params` is typed `Params`",
        ),
        (
            "a turbofish path",
            r"
            use axum::extract::Path;
            pub async fn turbofish(Path(id): Path::<String>) {}
            ",
            "fixture.rs: turbofish: parameter `Path(id)` is typed `Path::<String>`",
        ),
        (
            "a bare body",
            r"
            use axum::body::Bytes;
            pub async fn raw(body: Bytes) {}
            ",
            "fixture.rs: raw: parameter `body` is typed `Bytes`",
        ),
        (
            "an extractor in an unknown wrapper",
            r"
            use axum::extract::Path;
            pub async fn optional(id: Option<Path<String>>) {}
            ",
            "fixture.rs: optional: parameter `id` is typed `Option<Path<String>>`",
        ),
        (
            "a result whose error is not a rejection",
            r"
            use axum::extract::Path;
            pub async fn mapped(id: Result<Path<i64>, ServerError>) {}
            ",
            "fixture.rs: mapped: parameter `id` is typed `Result<Path<i64>, ServerError>`",
        ),
    ];
    for (what, source, offence) in fixtures {
        assert_eq!(
            offences_of_every_fn("fixture.rs", source),
            [offence],
            "{what}"
        );
    }
}

/// The positive control: every extractor shape a handler may take passes,
/// aliases of them included, and so does a function the scan was not
/// asked about.
#[test]
fn the_scanner_passes_deferred_and_non_rejecting_extractors() {
    let source = r"
        use axum::extract::rejection::JsonRejection as BodyRejection;
        use axum::extract::{Extension, Path, Query, RawQuery, State as AppStateOf};
        use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};

        pub async fn wrapped(
            AppStateOf(state): AppStateOf<AppState>,
            Extension(verified): Extension<VerifiedKey>,
            headers: axum::http::HeaderMap,
            id: Result<Path<i64>, PathRejection>,
            params: Result<Query<P>, axum::extract::rejection::QueryRejection>,
            pair: (State<AppState>, Extension<RequestId>),
            raw: RawQuery,
            body: Result<Json<Req>, BodyRejection>,
        ) -> Json<Out> {
            todo!()
        }

        pub async fn ingest(
            IngestKey(verified): IngestKey,
            body: Result<Bytes, BytesRejection>,
        ) {}

        pub async fn whole(request: axum::extract::Request) {}

        async fn helper(state: &AppState, id: u64) {}
        // pub async fn only_in_a_comment(Path(x): Path<i64>) {}
    ";
    assert_eq!(
        offences_in("fixture.rs", source, &["wrapped", "ingest", "whole"]),
        Vec::<String>::new()
    );
}
