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
//! guard does not read. A registration it cannot follow to its handlers,
//! such as `on(…)`, a macro, or a router nested from elsewhere, fails it
//! too, named by its path and expression. A `.nest(…)` or `.merge(…)` of a
//! name is followed through every binding of that name in `router`, and
//! each must build a router whose own registrations the scan reads: a
//! chain on `Router::new()`, or on a name bound the same way. A binding to
//! anything else, or a use of the name the scan cannot follow, fails the
//! guard. In each registered handler it fails, naming the
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
//! A second rule keeps the body of a key without the route's permission
//! unread. axum runs extractors in parameter order, and a body extractor
//! reads the body whatever the handler does with its result, so in each
//! registered handler a parameter that reads the body must come after an
//! authority extractor ([`AUTHORITY`]: `Authorized<_>` or `IngestKey`),
//! which refuses the key on its request head. A `Result` parameter reads
//! the body unless its extractor is on [`HEAD_ONLY_RESULTS`], so a body
//! extractor the scan does not know fails too. A tuple parameter's members
//! are judged in extraction order, as parameters of their own. A raw
//! `Request` or `Body` ([`RAW`]) reads the body where the scan cannot see,
//! and no authenticated handler takes one, so the rule refuses it outright,
//! after an authority extractor too. The scan names the file, the function
//! and the parameter.
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
/// request and body, which the body rule refuses ([`RAW`]), and the
/// authority extractors, whose refusal is the route's authority check.
const NON_REJECTING: &[&str] = &[
    "State",
    "Extension",
    "HeaderMap",
    "RawQuery",
    "Request",
    "Body",
    "Authorized",
    "IngestKey",
];

/// The authority extractors, by the last segment of their type's path:
/// request-parts extractors that refuse a key without the route's
/// permission on the request head, before any body extractor runs.
const AUTHORITY: &[&str] = &["Authorized", "IngestKey"];

/// The extractors a handler takes as `Result<_, …Rejection>` that read the
/// request head alone. Every other `Result` parameter is judged to read
/// the body, so the body rule fails closed.
const HEAD_ONLY_RESULTS: &[&str] = &["Path", "Query"];

/// The raw request and body, by the last segment of their type's path.
/// Either hands the handler a body to read itself, out of the scan's
/// sight, and no authenticated handler takes one, so the body rule refuses
/// them wherever they stand.
const RAW: &[&str] = &["Request", "Body"];

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
    split_top_level_at(text, ',')
}

/// Split `text` at each `separator` that is not inside `()`, `<>`, `[]` or
/// `{}`.
fn split_top_level_at(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    let mut previous = ' ';
    for (at, c) in text.char_indices() {
        match c {
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            // `->` and `=>` are not closing angle brackets.
            '>' if previous != '-' && previous != '=' => depth -= 1,
            c if c == separator && depth == 0 => {
                parts.push(&text[start..at]);
                start = at + c.len_utf8();
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
    enclosed(text, open, '(', ')')
}

/// The text between the `{` at `open` in `text` and its matching `}`.
fn braced(text: &str, open: usize) -> Option<&str> {
    enclosed(text, open, '{', '}')
}

/// The text between the `opening` at `open` in `text` and its matching
/// `closing`.
fn enclosed(text: &str, open: usize, opening: char, closing: char) -> Option<&str> {
    let mut depth = 0_i32;
    for (at, c) in text[open..].char_indices() {
        if c == opening {
            depth += 1;
        } else if c == closing {
            depth -= 1;
            if depth == 0 {
                return Some(&text[open + 1..open + at]);
            }
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

/// Whether a parameter typed `ty` reads the request body: a `Result`
/// whose extractor is not on [`HEAD_ONLY_RESULTS`], such as
/// `Result<Json<_>, _>` or `Result<Bytes, _>`.
fn reads_the_body(ty: &str, aliases: &[(String, String)]) -> bool {
    let ty = ty.trim();
    if resolved(outer_name(ty), aliases) != "Result" {
        return false;
    }
    match split_top_level(generic_args(ty)).as_slice() {
        [extracted, _] => {
            !HEAD_ONLY_RESULTS.contains(&resolved(outer_name(extracted.trim()), aliases))
        }
        _ => true,
    }
}

/// The extractors a parameter typed `ty` runs, in the order axum runs
/// them: each member of a tuple, nested tuples flattened, or `ty` itself.
fn extractors(ty: &str) -> Vec<&str> {
    let ty = ty.trim();
    match ty.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        Some(inner) => split_top_level(inner)
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .flat_map(extractors)
            .collect(),
        None => vec![ty],
    }
}

/// One line per body parameter of the handlers `handlers` names in
/// `source`, which `file` names, that no authority extractor precedes,
/// and one per raw request or body ([`RAW`]). axum runs extractors in
/// parameter order, a tuple's members in theirs, so only an authority
/// extractor ahead of the body refuses a key before its body is read.
fn bodies_ahead_of_authority_in(file: &str, source: &str, handlers: &[&str]) -> Vec<String> {
    let aliases = use_aliases(source);
    let mut offences = Vec::new();
    for (name, params) in async_fns(source) {
        if !handlers.contains(&name.as_str()) {
            continue;
        }
        let mut authorized = false;
        for param in split_top_level(&params) {
            let Some((pattern, ty)) = split_parameter(param) else {
                continue;
            };
            for extractor in extractors(ty) {
                let extracted = resolved(outer_name(extractor), &aliases);
                if RAW.contains(&extracted) {
                    offences.push(format!(
                        "{file}: {name}: parameter `{}` takes the raw `{extracted}`, which no \
                         authenticated handler may take",
                        pattern.trim()
                    ));
                } else if AUTHORITY.contains(&extracted) {
                    authorized = true;
                } else if !authorized && reads_the_body(extractor, &aliases) {
                    offences.push(format!(
                        "{file}: {name}: parameter `{}` reads the body before an authority \
                         extractor",
                        pattern.trim()
                    ));
                }
            }
        }
    }
    offences
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
/// as `handlers::query` or `ingest::preview::preview`, each once. Fails,
/// naming each, on any registration the guard cannot follow to its
/// handlers.
fn registered_handlers(router: &str) -> Vec<String> {
    let (handlers, unreadable) = registrations(router);
    assert!(
        unreadable.is_empty(),
        "transport/http.rs makes {} registration(s) this guard cannot follow to a handler. \
         Register each handler as `.route(path, get(module::handler))`, chaining \
         `.post(…)` and the other methods, and nest or merge only a router bound \
         with `let` in `router` to a chain on `Router::new()`:\n{}",
        unreadable.len(),
        unreadable.join("\n")
    );
    handlers
}

/// What the registrations of `router` name: the handler paths of its
/// `.route(…)` calls, each once, and one line per registration the guard
/// cannot read, the call as written. A `.route(…)` is read when its
/// expression is a chain of [`METHODS`] calls on handler paths, such as
/// `put(routes::c).delete(routes::delete)`. A `.nest(…)` or `.merge(…)` is
/// read when it names a router [`follows_router`] follows, whose own
/// `.route(…)` calls are in `router` and so are scanned. Every other
/// registration in [`OTHER_REGISTRATIONS`], and any `.route(…)` expression
/// the chain does not cover, is unreadable: the scan fails closed.
fn registrations(router: &str) -> (Vec<String>, Vec<String>) {
    let mut handlers: Vec<String> = Vec::new();
    let mut unreadable = Vec::new();
    for args in calls(router, "route") {
        let named = match top_level_args(args).as_slice() {
            [_, expr] => method_chain(expr),
            _ => None,
        };
        let Some(named) = named else {
            unreadable.push(format!("`.route({})`", collapsed(args)));
            continue;
        };
        for handler in named {
            if !handlers.iter().any(|h| h == handler) {
                handlers.push(handler.to_owned());
            }
        }
    }
    for registration in OTHER_REGISTRATIONS {
        for args in calls(router, registration) {
            let bound = match (*registration, top_level_args(args).as_slice()) {
                ("nest", [_, inner]) | ("merge", [inner]) => {
                    let inner = inner.trim();
                    is_identifier(inner) && follows_router(router, inner, &mut Vec::new())
                }
                _ => false,
            };
            if !bound {
                unreadable.push(format!("`.{registration}({})`", collapsed(args)));
            }
        }
    }
    (handlers, unreadable)
}

/// Whether `text` is one identifier, such as a `let` binding's name.
fn is_identifier(text: &str) -> bool {
    text.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether the router `name` names in `router` is one whose registrations
/// are all in `router`, where the scan reads them: `name` is bound at least
/// once ([`bindings`]), and every binding builds such a router
/// ([`builds_a_followed_router`]). `seen` holds the names already being
/// followed: a chain on one of them, as on `name` itself as it was bound
/// before, is judged by that name's own bindings.
fn follows_router(router: &str, name: &str, seen: &mut Vec<String>) -> bool {
    if seen.iter().any(|s| s == name) {
        return true;
    }
    seen.push(name.to_owned());
    bindings(router, name).is_some_and(|bound| {
        !bound.is_empty()
            && bound
                .into_iter()
                .all(|value| builds_a_followed_router(value, router, seen))
    })
}

/// Whether `value`, what a binding assigns, builds a router whose
/// registrations are all in `router`: a chain of method calls on
/// `Router::new()`, or on a name [`follows_router`] follows, or an
/// `if … else` whose every block ends in one.
fn builds_a_followed_router(value: &str, router: &str, seen: &mut Vec<String>) -> bool {
    let value = value.trim();
    if let Some(branches) = value.strip_prefix("if ") {
        return block_values(branches).is_some_and(|values| {
            values
                .into_iter()
                .all(|value| builds_a_followed_router(value, router, seen))
        });
    }
    match chain_receiver(value) {
        Some("Router::new()") => true,
        Some(receiver) if receiver.ends_with("::Router::new()") => true,
        Some(receiver) if is_identifier(receiver) => follows_router(router, receiver, seen),
        _ => false,
    }
}

/// The receiver of a method chain such as `Router::new().route(…)` or
/// `routes.layer(…)`: a path, called once with no arguments or not at all,
/// then nothing but `.method(…)` calls. `None` for anything else.
fn chain_receiver(value: &str) -> Option<&str> {
    let path_end = value
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'))
        .unwrap_or(value.len());
    let receiver_end = if value[path_end..].starts_with("()") {
        path_end + 2
    } else {
        path_end
    };
    if path_end == 0 {
        return None;
    }
    let mut rest = &value[receiver_end..];
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(&value[..receiver_end]);
        }
        rest = rest.strip_prefix('.')?.trim_start();
        let name_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let after = &rest[name_end..];
        if name_end == 0 || !after.starts_with('(') {
            return None;
        }
        let args = parenthesized(after, 0)?;
        rest = &after[args.len() + 2..];
    }
}

/// The value each block of `if <condition> { … } else { … }` ends in, the
/// text after its `if`, `else if` chains included. `None` unless every
/// block ends in a value and an `else` block closes the chain.
fn block_values(branches: &str) -> Option<Vec<&str>> {
    let open = branches.find('{')?;
    let block = braced(branches, open)?;
    let mut values = vec![block_value(block)?];
    let rest = branches[open + block.len() + 2..]
        .trim_start()
        .strip_prefix("else")?
        .trim_start();
    if let Some(chained) = rest.strip_prefix("if ") {
        values.extend(block_values(chained)?);
        return Some(values);
    }
    if !rest.starts_with('{') {
        return None;
    }
    let block = braced(rest, 0)?;
    if !rest[block.len() + 2..].trim().is_empty() {
        return None;
    }
    values.push(block_value(block)?);
    Some(values)
}

/// What a block ends in: the text after its last top-level `;`, or `None`
/// when that is empty.
fn block_value(block: &str) -> Option<&str> {
    let value = split_top_level_at(block, ';').pop()?;
    (!value.trim().is_empty()).then_some(value)
}

/// What every binding of `name` in `router` assigns, or `None` when `name`
/// appears anywhere the scan cannot follow. A name may appear only
///
/// - where `let`, `let mut` or a plain assignment binds it, with or without
///   a type: the value runs to the binding's `;`;
/// - as the receiver of a method call;
/// - as the whole value of a block, as `else { name }`; and
/// - as the router a `.merge(name)` or `.nest(path, name)` takes.
///
/// Any other use, such as a pattern that destructures it, a closure or
/// `for` that binds it, or a call that is handed it, is `None`.
fn bindings<'a>(router: &'a str, name: &str) -> Option<Vec<&'a str>> {
    let mut bound = Vec::new();
    let mut from = 0;
    while let Some(found) = router[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        from = end;
        let before = &router[..start];
        let after = &router[end..];
        let part_of_a_longer_name = before
            .ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == ':')
            || after.starts_with(|c: char| c.is_alphanumeric() || c == '_');
        if part_of_a_longer_name {
            continue;
        }
        let before = before.trim_end();
        let after = after.trim_start();
        if let Some(value) = binding_value(before, after) {
            bound.push(value);
        } else if !(after.starts_with('.') && !after.starts_with("..")
            || before.ends_with('{') && after.starts_with('}')
            || after.starts_with(')') && takes_a_router(before))
        {
            return None;
        }
    }
    Some(bound)
}

/// The value assigned by a binding of a name between `before` and `after`,
/// or `None` when it is no binding: `let`, `let mut` or the start of a
/// statement before it, then `=` or a type and `=` after it.
fn binding_value<'a>(before: &str, after: &'a str) -> Option<&'a str> {
    let binds = ends_with_word(before, "let")
        || before
            .strip_suffix("mut")
            .is_some_and(|b| ends_with_word(b.trim_end(), "let"))
        || before.is_empty()
        || before.ends_with([';', '{', '}']);
    if !binds {
        return None;
    }
    let assigned = if let Some(typed) = after.strip_prefix(':') {
        if typed.starts_with(':') {
            return None;
        }
        &typed[typed.find('=')?..]
    } else {
        after
    };
    let value = assigned.strip_prefix('=')?;
    if value.starts_with(['=', '>']) {
        return None;
    }
    split_top_level_at(value, ';').into_iter().next()
}

/// Whether `text` ends in the keyword `word`, not in a longer name.
fn ends_with_word(text: &str, word: &str) -> bool {
    text.strip_suffix(word)
        .is_some_and(|rest| !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// Whether the text `before` a name ends where `.merge(` or `.nest(path,`
/// takes it as its router.
fn takes_a_router(before: &str) -> bool {
    if before.ends_with(".merge(") {
        return true;
    }
    let Some(path_end) = before.strip_suffix(',') else {
        return false;
    };
    let mut depth = 0_i32;
    for (at, c) in path_end.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' if depth == 0 => {
                return path_end[..at]
                    .strip_suffix("nest")
                    .is_some_and(|call| call.ends_with('.'));
            }
            '(' => depth -= 1,
            _ => {}
        }
    }
    false
}

/// The registrations a router makes besides `.route(…)`.
const OTHER_REGISTRATIONS: &[&str] = &[
    "nest",
    "merge",
    "nest_service",
    "route_service",
    "fallback",
    "fallback_service",
];

/// The argument list of every `.{name}(…)` call in `text`, in order.
fn calls<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let call = format!(".{name}(");
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&call) {
        rest = &rest[at + call.len() - 1..];
        found.push(parenthesized(rest, 0).expect("a registration without its `)`"));
        rest = &rest[1..];
    }
    found
}

/// The non-empty top-level arguments of an argument list.
fn top_level_args(args: &str) -> Vec<&str> {
    split_top_level(args)
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect()
}

/// `text` with each run of whitespace made one space.
fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The handler paths of a method chain such as `get(a)` or
/// `put(a).delete(b)`, or `None` when `expr` is anything else: another
/// call, a macro, a method this guard does not know, or a handler that is
/// not a path.
fn method_chain(expr: &str) -> Option<Vec<&str>> {
    let mut handlers = Vec::new();
    let mut rest = expr.trim();
    loop {
        let name_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let (name, after) = rest.split_at(name_end);
        if !METHODS.contains(&name) || !after.starts_with('(') {
            return None;
        }
        let inside = parenthesized(after, 0)?;
        let handler = inside.trim();
        if handler.is_empty()
            || !handler
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
        {
            return None;
        }
        handlers.push(handler);
        rest = after[inside.len() + 2..].trim_start();
        if rest.is_empty() {
            return Some(handlers);
        }
        rest = rest.strip_prefix('.')?.trim_start();
    }
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

/// No authenticated handler reads its body before an authority extractor
/// has judged the key, so the body of a key without the route's permission
/// is never read.
#[test]
fn every_authenticated_body_follows_an_authority_extractor() {
    let handlers = authenticated_handlers();
    let mut offences = Vec::new();
    for file in SOURCES {
        let names: Vec<&str> = handlers
            .iter()
            .filter(|(f, _)| f == file)
            .map(|(_, name)| name.as_str())
            .collect();
        offences.extend(bodies_ahead_of_authority_in(file, &read(file), &names));
    }
    assert!(
        offences.is_empty(),
        "{} handler parameter(s) read the body before the route's authority is judged, or \
         take the raw request or body (ADR-0059). Bind `policy::Authorized<needs::…>` ahead \
         of the body parameter and drop the check in the handler's body; read the body \
         through an extractor, not the raw request:\n{}",
        offences.len(),
        offences.join("\n")
    );
}

/// The body rule's negative controls: a body ahead of its authority
/// extractor, a body with none, a body inside a tuple ahead of it, and a
/// raw request or body anywhere are each one offence.
#[test]
fn the_body_rule_flags_a_body_ahead_of_its_authority() {
    let fixtures = [
        (
            "a body before the authority extractor",
            r"
            use axum::extract::rejection::JsonRejection;
            pub async fn early(
                body: Result<Json<Req>, JsonRejection>,
                verified: Authorized<needs::Query>,
            ) {}
            ",
            "fixture.rs: early: parameter `body` reads the body before an authority extractor",
        ),
        (
            "a body with no authority extractor",
            r"
            use axum::extract::rejection::BytesRejection;
            pub async fn unguarded(
                State(state): State<AppState>,
                Extension(verified): Extension<VerifiedKey>,
                body: Result<Bytes, BytesRejection>,
            ) {}
            ",
            "fixture.rs: unguarded: parameter `body` reads the body before an authority \
             extractor",
        ),
        (
            "another body extractor, judged closed",
            r"
            use axum::extract::rejection::StringRejection;
            pub async fn text(body: Result<String, StringRejection>) {}
            ",
            "fixture.rs: text: parameter `body` reads the body before an authority extractor",
        ),
        (
            "the raw request, even after the authority extractor",
            r"
            pub async fn whole(_: Authorized<needs::Query>, request: axum::extract::Request) {}
            ",
            "fixture.rs: whole: parameter `request` takes the raw `Request`, which no \
             authenticated handler may take",
        ),
        (
            "the raw body under another name",
            r"
            use axum::body::Body as Raw;
            pub async fn raw(verified: Authorized<needs::Query>, body: Raw) {}
            ",
            "fixture.rs: raw: parameter `body` takes the raw `Body`, which no authenticated \
             handler may take",
        ),
        (
            "a body inside a tuple, ahead of any authority extractor",
            r"
            use axum::extract::rejection::{JsonRejection, PathRejection};
            pub async fn paired(
                (id, body): (Result<Path<i64>, PathRejection>, Result<Json<Req>, JsonRejection>),
                verified: Authorized<needs::Query>,
            ) {}
            ",
            "fixture.rs: paired: parameter `(id, body)` reads the body before an authority \
             extractor",
        ),
    ];
    for (what, source, offence) in fixtures {
        let fns = async_fns(source);
        let names: Vec<&str> = fns.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            bodies_ahead_of_authority_in("fixture.rs", source, &names),
            [offence],
            "{what}"
        );
    }
}

/// The body rule's positive control: a body after `Authorized` or
/// `IngestKey`, an aliased extractor and a tuple's earlier member
/// included, passes, and so does a path or query string ahead of it.
#[test]
fn the_body_rule_passes_a_body_after_its_authority() {
    let source = r"
        use axum::extract::rejection::{BytesRejection, JsonRejection, PathRejection};
        use crate::policy::Authorized as Key;

        pub async fn guarded(
            State(state): State<AppState>,
            id: Result<Path<i64>, PathRejection>,
            verified: Authorized<needs::SavedQuery>,
            body: Result<Json<Req>, JsonRejection>,
        ) {}

        pub async fn aliased(_: Key<needs::Export>, body: Result<Json<Req>, JsonRejection>) {}

        pub async fn ingest(
            IngestKey(verified): IngestKey,
            body: Result<Bytes, BytesRejection>,
        ) {}

        pub async fn headless(id: Result<Path<i64>, PathRejection>) {}

        pub async fn paired(
            (verified, body): (Authorized<needs::Query>, Result<Json<Req>, JsonRejection>),
        ) {}

        pub async fn split(
            (id, verified): (Result<Path<i64>, PathRejection>, Key<needs::Query>),
            body: Result<Json<Req>, JsonRejection>,
        ) {}
    ";
    assert_eq!(
        bodies_ahead_of_authority_in(
            "fixture.rs",
            source,
            &[
                "guarded", "aliased", "ingest", "headless", "paired", "split"
            ]
        ),
        Vec::<String>::new()
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

/// The router parser: method chains, nested module paths, comments, and a
/// nest or merge of a router bound in the same function, to a chain on
/// `Router::new()`, to an `if … else` of such chains, or to a chain on
/// itself as it was bound before.
#[test]
fn the_router_parser_finds_each_registration() {
    let router = without_line_comments(
        r#"
        let nested = Router::new().route("/e", get(handlers::e));
        let nested = if on { nested.route("/f", get(handlers::f)) } else { nested };
        let nested = nested
            .layer(middleware::from_fn_with_state(Limit { bytes: max }, limit))
            .layer(DefaultBodyLimit::disable());
        let merged = if on {
            let limit = http.limit.unwrap_or(DEFAULT);
            Router::new().route("/g", post(handlers::g)).layer(limit)
        } else {
            Router::new()
        };
        let app = Router::new()
            .route("/a", post(handlers::a).get(handlers::b))
            .route(
                "/c/{id}",
                put(routes::c)
                    .delete(routes::delete),
            )
            // .route("/hidden", get(handlers::hidden))
            .route("/d", get(ingest::preview::d))
            .route_layer(middleware::from_fn(failure::mark_handler))
            .nest("/api/v1", nested)
            .merge(merged);
        "#,
    );
    assert_eq!(
        registered_handlers(&router),
        [
            "handlers::e",
            "handlers::f",
            "handlers::g",
            "handlers::a",
            "handlers::b",
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

/// Negative controls for the router parser: each registration it cannot
/// follow to a handler fails the guard, named by its call, path and
/// expression included, instead of leaving the scan.
#[test]
fn the_router_parser_refuses_each_registration_it_cannot_read() {
    let fixtures = [
        (
            "a method filter",
            r#".route("/on", on(MethodFilter::GET, handlers::on))"#,
        ),
        ("a route macro", r#".route("/m", routes!(handlers::m))"#),
        (
            "a chain with a link it cannot read",
            r#".route("/c", get(handlers::c).on(MethodFilter::POST, handlers::d))"#,
        ),
        (
            "a method router built elsewhere",
            r#".route("/r", routes::method_router())"#,
        ),
        (
            "a nested router built elsewhere",
            r#".nest("/n", investigations::router())"#,
        ),
        (
            "a merged router built elsewhere",
            r".merge(results::router())",
        ),
        (
            "a service",
            r#".route_service("/s", ServeDir::new("assets"))"#,
        ),
    ];
    for (what, registration) in fixtures {
        let router = format!(
            "let app = Router::new()\n    .route(\"/ok\", get(handlers::ok))\n    {registration};"
        );
        let (handlers, unreadable) = registrations(&without_line_comments(&router));
        assert_eq!(unreadable, [format!("`{registration}`")], "{what}");
        assert!(handlers.contains(&"handlers::ok".to_owned()), "{what}");
    }

    let bound = [
        (
            "a merged router bound to one built elsewhere",
            "let hidden = other_router();",
            ".merge(hidden)",
        ),
        (
            "a nested router bound to one built elsewhere",
            "let hidden = routes::router().layer(cors);",
            r#".nest("/n", hidden)"#,
        ),
        (
            "a typed binding to a router built elsewhere",
            "let hidden: Router<AppState> = other_router();",
            ".merge(hidden)",
        ),
        (
            "a router rebound to one built elsewhere",
            "let mut hidden = Router::new();\nhidden = other_router();",
            ".merge(hidden)",
        ),
        (
            "a router shadowed by a pattern",
            "let hidden = Router::new();\nlet (hidden, _) = (other_router(), 0);",
            ".merge(hidden)",
        ),
        (
            "a router a closure binds",
            "let hidden = Router::new();\nlet wrap = |hidden: Router| hidden;",
            ".merge(hidden)",
        ),
        (
            "a router an if binds to one built elsewhere",
            "let hidden = if on { Router::new() } else { other_router() };",
            ".merge(hidden)",
        ),
        (
            "a router chained on one bound elsewhere",
            "let inner = other_router();\nlet hidden = inner.route(\"/x\", get(handlers::x));",
            ".merge(hidden)",
        ),
        ("a router bound nowhere", "", ".merge(hidden)"),
    ];
    for (what, prelude, registration) in bound {
        let router = format!(
            "{prelude}\nlet app = Router::new()\n    .route(\"/ok\", get(handlers::ok))\n    \
             {registration};"
        );
        let (_, unreadable) = registrations(&without_line_comments(&router));
        assert_eq!(unreadable, [format!("`{registration}`")], "{what}");
    }
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

        async fn helper(state: &AppState, id: u64) {}
        // pub async fn only_in_a_comment(Path(x): Path<i64>) {}
    ";
    assert_eq!(
        offences_in("fixture.rs", source, &["wrapped", "ingest"]),
        Vec::<String>::new()
    );
}
