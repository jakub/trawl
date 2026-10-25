// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A source guard for ADR-0059's rule that an authenticated route judges
//! authority before request shape.
//!
//! A handler that binds `Path<_>`, `Query<_>` or `Json<_>` bare lets axum
//! answer its plain-text 400 or 422 before the handler runs, so a key
//! without the route's permission learns whether its request parsed. Every
//! handler takes those as `Result<_, …Rejection>`, checks its authority,
//! and only then maps the rejection through `shape::shaped`.
//!
//! This test reads the handler sources and fails, naming the file, the
//! function and the parameter, on any `async fn` parameter typed bare
//! `Path<…>`, `Query<…>` or `Json<…>`. The behavioral check of each route
//! is `every_handlers_route_judges_authority_before_request_shape` in
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
];

/// Handlers mounted outside the `authenticated` router in
/// `transport/http.rs` (`/api/v1/health`, `/metrics`). `/ingest` takes its
/// body as `Bytes` and lives in `ingest/handler.rs`, which is not scanned.
const UNAUTHENTICATED: &[&str] = &["health", "prometheus_metrics"];

/// Drop `//` comments, so prose that mentions `async fn` is not a function.
fn without_line_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| line.find("//").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split `text` at commas that are not inside `()`, `<>` or `[]`.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    let mut previous = ' ';
    for (at, c) in text.char_indices() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
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

/// The last path segment before the first `<` of `ty`: `axum::Json<T>` is
/// `Json`. A type with no `<` is returned whole.
fn outer_name(ty: &str) -> &str {
    let head = ty.split('<').next().unwrap_or(ty).trim();
    head.rsplit("::").next().unwrap_or(head)
}

/// The text between the first `<` of `ty` and its matching `>`.
fn generic_args(ty: &str) -> &str {
    let Some(open) = ty.find('<') else { return "" };
    let close = ty.rfind('>').unwrap_or(ty.len());
    &ty[open + 1..close]
}

/// Whether `ty` is a request extractor that rejects before the handler
/// runs: a bare `Path`, `Query` or `Json`, or a `Result` of one whose error
/// is not an axum `…Rejection`.
fn rejects_before_the_handler(ty: &str) -> bool {
    let extractor = |name: &str| matches!(name, "Path" | "Query" | "Json");
    let name = outer_name(ty);
    if extractor(name) {
        return true;
    }
    if name != "Result" {
        return false;
    }
    let args = split_top_level(generic_args(ty));
    let [ok, err] = args.as_slice() else {
        return false;
    };
    extractor(outer_name(ok.trim())) && !err.trim().ends_with("Rejection")
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
        let mut depth = 0_i32;
        let mut close = None;
        for (at, c) in rest[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + at);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { break };
        found.push((name, rest[open + 1..close].to_owned()));
        rest = &rest[close..];
    }
    found
}

/// One line per offending parameter in `source`, which `file` names.
fn bare_extractors(file: &str, source: &str) -> Vec<String> {
    let mut offences = Vec::new();
    for (name, params) in async_fns(source) {
        if UNAUTHENTICATED.contains(&name.as_str()) {
            continue;
        }
        for param in split_top_level(&params) {
            let Some((pattern, ty)) = split_parameter(param) else {
                continue;
            };
            if rejects_before_the_handler(ty.trim()) {
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

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// No authenticated handler binds `Path`, `Query` or `Json` bare.
#[test]
fn no_authenticated_handler_binds_a_bare_extractor() {
    let mut offences = Vec::new();
    for file in SOURCES {
        let source = read(file);
        assert!(
            !async_fns(&source).is_empty(),
            "{file}: no `async fn` found, so the scan would pass for nothing"
        );
        offences.extend(bare_extractors(file, &source));
    }
    assert!(
        offences.is_empty(),
        "{} handler parameter(s) take a bare extractor, so axum answers its own \
         400 before the authority check (ADR-0059). Take it as \
         `Result<_, …Rejection>`, check authority, then pass it through \
         `shape::shaped`:\n{}",
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

/// The scanner itself: it flags each bare extractor and nothing else.
#[test]
fn the_scanner_flags_bare_extractors_and_only_those() {
    let source = r"
        pub async fn bare(
            State(state): State<AppState>,
            Path(id): Path<i64>,
            axum::extract::Query(params): axum::extract::Query<P>,
            Json(req): Json<Req>,
        ) -> Result<Json<Out>, ServerError> {}

        async fn pair(Path((a, b)): Path<(i64, i64)>) {}

        async fn wrapped(
            id: Result<Path<i64>, PathRejection>,
            params: Result<Query<P>, axum::extract::rejection::QueryRejection>,
            body: Result<Json<Req>, JsonRejection>,
            Extension(v): Extension<Key>,
            raw: RawQuery,
        ) -> Json<Out> {}

        async fn mapped(id: Result<Path<i64>, ServerError>) {}
        // async fn only_in_a_comment(Path(x): Path<i64>) {}
    ";
    let offences = bare_extractors("fixture.rs", source);
    assert_eq!(
        offences,
        vec![
            "fixture.rs: bare: parameter `Path(id)` is typed `Path<i64>`",
            "fixture.rs: bare: parameter `axum::extract::Query(params)` is typed `axum::extract::Query<P>`",
            "fixture.rs: bare: parameter `Json(req)` is typed `Json<Req>`",
            "fixture.rs: pair: parameter `Path((a, b))` is typed `Path<(i64, i64)>`",
            "fixture.rs: mapped: parameter `id` is typed `Result<Path<i64>, ServerError>`",
        ]
    );
}
