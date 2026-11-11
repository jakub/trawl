#!/usr/bin/env python3
"""List every INFO, WARN and ERROR emission in trawld's non-test code.

Usage, from the repository root:

    python3 -I docs/evidence/issue-412/enumerate.py           # write retained-events.tsv
    python3 -I docs/evidence/issue-412/enumerate.py --check   # diff against the committed file
    python3 -I docs/evidence/issue-412/enumerate.py --stdout  # print, write nothing
    python3 -I docs/evidence/issue-412/enumerate.py --hits    # search hits outside rows and DEBUG events
    python3 -I docs/evidence/issue-412/enumerate.py --helpers # functions whose event reads a parameter

The script walks the module tree of `crates/trawl-server` from `src/lib.rs`
and `src/main.rs`. It drops every item that a `#[cfg(...)]` attribute
removes from a production build: a predicate that is false when `test` and
every `feature = "..."` are false. That covers `#[cfg(test)]`,
`#[cfg(any(test, feature = "test-support"))]` and a `mod x;` declared under
either, whose whole file is then skipped. Other cfg atoms (unix, target_os)
are unknown and keep their code.

It finds each `info!`, `warn!` and `error!`, with or without a `tracing::`
prefix, outside comments and string literals, and takes the whole macro
body by bracket matching. A `macro_rules!` body that calls
`tracing::$level!` is expanded at each invocation that names info, warn or
error. Any `event!` macro or `log::` logging macro stops the run: the
script does not read them, and none exists today.

A logging helper is a function with an INFO+ event that takes an error
value from its caller, whether it logs the text, the class or neither.
HELPERS names each one; the script adds a row for every call, at the
call's line. The row's level, target and event_type are the helper's
(joined with `/` when it has several events), narrowed to the level a
`Loudness::Warn`-style argument picks and the event_type literal the call
passes; the source-type column names the helper.

The human columns (source type, disposition) live in dispositions.tsv,
keyed by file, event_type, level, kind and the ordinal of that key in the
file. A line shift does not change a key. The script exits non-zero, naming
the row, when a site has no disposition, a disposition has no site, a
disposition cell is blank, or a disposition is not one of `changed`,
`unchanged: <reason>` or `D2b`.

Known limits:
- The default target is the module path the file sits at, plus any inline
  `mod name { }` around the macro. An explicit `target:` resolves a string
  literal or a `&str` constant declared in this crate; anything else stops
  the run.
- `event_type` is the literal after `event_type =`. A non-literal value is
  written `{expr}`, with the expression's text; `-` means the event has no
  event_type field.
- Spans (`info_span!` and friends) and DEBUG or TRACE events are not rows.
- A helper is only recognised if HELPERS names it. `--hits` prints search
  hits outside any row's macro body or helper call, for a person to read.
- A helper's calls are matched by name in its own file, or by HELPERS'
  pattern in every file. A call by name from another file without a
  pattern stops the run. A METHOD helper's calls are matched in its own
  file only, since other types share the name. A call through a function
  pointer is not seen.
"""

import argparse
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
CRATE = os.path.join(REPO, "crates", "trawl-server")
SRC = os.path.join(CRATE, "src")
OUT = os.path.join(HERE, "retained-events.tsv")
DISPOSITIONS = os.path.join(HERE, "dispositions.tsv")

SEARCH = (
    "rg -n --type rust "
    "'(error|err|reason|cause|source|detail)\\s*=\\s*[%?]|\\{(e|err|error)\\}' "
    "crates/trawl-server/src"
)
SEARCH_RE = re.compile(r"(error|err|reason|cause|source|detail)\s*=\s*[%?]|\{(e|err|error)\}")

LEVELS = ("info", "warn", "error")

# Logging helpers that take an error value from their caller, whether the
# helper logs its text, its class or nothing of it at INFO and above. Each
# entry is (file that defines it, function name, call pattern). With no
# pattern, a call is the bare name, `self.name(` or `Self::name(` in the
# defining file, and the run stops if the name is called in any other file.
# A pattern is searched in every file. METHOD marks a method whose name
# other types also use: its calls are matched in its own file only.
# `--helpers` lists the candidates a
# person picks these from.
METHOD = "method"

HELPERS = [
    ("catalog/conform.rs", "skip_file", None),
    ("catalog/conform.rs", "skip_failed", None),
    ("env_dirs.rs", "warn_unlisted", None),
    ("error.rs", "auth_backend_fault", None),
    ("error.rs", "log_fault", METHOD),
    ("ingest/compaction.rs", "duckdb", r"\bStatementFailure::duckdb\s*\("),
    ("ingest/pipeline.rs", "record_discarded_group", None),
    ("ingest/publication_marker.rs", "log_outcome", None),
    ("investigations/step.rs", "log", METHOD),
    ("live.rs", "log_missing_prepared_pattern", None),
    ("main.rs", "log_crash_dump", None),
    ("publication.rs", "log_removal_sync_owed", None),
    ("query_log.rs", "check_owner_only", None),
    ("repin/engine.rs", "log_store_fault", None),
    ("results/charge.rs", "warn_retire", None),
    ("results/charge.rs", "warn_retire_store", None),
    ("results/compare.rs", "unlisted", METHOD),
    ("results/service.rs", "log_expire_failed", None),
    ("results/service.rs", "log_fenced", None),
    ("results/service.rs", "log_history_failed", None),
    ("results/service.rs", "log_mark_missing_failed", None),
    ("results/sweep.rs", "warn", None),
    ("results/sweep.rs", "warn_store", None),
    ("scheduler.rs", "log_parquet_engine_fault", None),
    ("scheduler.rs", "log_store_fault", None),
    ("scheduler.rs", "recover_ambiguous_finish", None),
    ("syslog/listener_error.rs", "record_failure", METHOD),
    ("tls.rs", "cert_details_unparsed", None),
]

COLUMNS = ["file", "line", "event_type", "target", "level", "source type", "disposition"]
KEY_COLUMNS = ["file", "event_type", "level", "kind", "ordinal", "source type", "disposition"]


class Failure(Exception):
    pass


# ---------------------------------------------------------------- lexing


def blank(text):
    """Return text with comments and literal contents replaced by spaces.

    Offsets and newlines are kept, so an offset into the result is an
    offset into the source. String delimiters stay, so a literal still
    reads as `"   "`; char literal contents go too.
    """
    out = list(text)
    n = len(text)
    i = 0

    def wipe(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            wipe(i, j)
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            wipe(i, j)
            i = j
        elif c in "rbc" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            m = re.match(r'(?:br|cr|r)(#*)"', text[i:])
            if m:
                hashes = m.group(1)
                start = i + m.end()
                end = text.find('"' + hashes, start)
                if end < 0:
                    raise Failure("unterminated raw string at offset %d" % i)
                wipe(start, end)
                i = end + 1 + len(hashes)
            elif text.startswith('b"', i) or text.startswith('c"', i):
                i = skip_string(text, i + 1, wipe)
            elif text.startswith("b'", i):
                i = skip_char(text, i + 1, wipe)
            else:
                i += 1
        elif c == '"':
            i = skip_string(text, i, wipe)
        elif c == "'":
            i = skip_char(text, i, wipe)
        else:
            i += 1
    return "".join(out)


def skip_string(text, i, wipe):
    j = i + 1
    while text[j] != '"':
        j += 2 if text[j] == "\\" else 1
    wipe(i + 1, j)
    return j + 1


def skip_char(text, i, wipe):
    # A char literal or a lifetime. 'x' and '\n' are literals; 'a without a
    # closing quote two characters on is a lifetime or label.
    if text[i + 1] == "\\":
        j = text.find("'", i + 2)
        wipe(i + 1, j)
        return j + 1
    if i + 2 < len(text) and text[i + 2] == "'":
        wipe(i + 1, i + 2)
        return i + 3
    return i + 1


OPEN = {"(": ")", "[": "]", "{": "}"}
CLOSE = {")", "]", "}"}


def match_bracket(code, i):
    """Offset of the bracket closing the one at code[i]."""
    depth = 0
    for j in range(i, len(code)):
        ch = code[j]
        if ch in OPEN:
            depth += 1
        elif ch in CLOSE:
            depth -= 1
            if depth == 0:
                return j
    raise Failure("unbalanced bracket at offset %d" % i)


def split_top(code, a, b):
    """Split code[a:b] at depth-zero commas, as (start, end) spans."""
    spans, depth, start = [], 0, a
    for j in range(a, b):
        ch = code[j]
        if ch in OPEN:
            depth += 1
        elif ch in CLOSE:
            depth -= 1
        elif ch == "," and depth == 0:
            spans.append((start, j))
            start = j + 1
    spans.append((start, b))
    return [(s, e) for s, e in spans if code[s:e].strip()]


def line_of(text, offset):
    return text.count("\n", 0, offset) + 1


# ---------------------------------------------------------------- cfg


def cfg_value(pred):
    """Three-valued: False when test and features are off, else True/None."""
    pred = pred.strip()
    m = re.fullmatch(r"(not|any|all)\s*\((.*)\)", pred, re.S)
    if m:
        op, inner = m.group(1), m.group(2)
        parts = [inner[s:e] for s, e in split_top(inner, 0, len(inner))]
        vals = [cfg_value(p) for p in parts]
        if op == "not":
            return None if vals[0] is None else not vals[0]
        if op == "any":
            if any(v is True for v in vals):
                return True
            return False if all(v is False for v in vals) else None
        if any(v is False for v in vals):
            return False
        return True if all(v is True for v in vals) else None
    if pred == "test" or re.fullmatch(r'feature\s*=\s*"[^"]*"', pred):
        return False
    return None


ATTR = re.compile(r"#\s*\[\s*cfg\s*\(")


def item_end(code, i):
    """End offset of the item that starts at code[i] (after its attributes)."""
    n = len(code)
    while True:
        while i < n and code[i].isspace():
            i += 1
        if code.startswith("#", i):
            k = code.find("[", i)
            i = match_bracket(code, k) + 1
        else:
            break
    depth = 0
    j = i
    while j < n:
        ch = code[j]
        if ch in OPEN:
            depth += 1
        elif ch in CLOSE:
            depth -= 1
            if depth < 0:
                return j
            if depth == 0 and ch == "}":
                k = j + 1
                while k < n and code[k].isspace():
                    k += 1
                if code.startswith("else", k) and not (code[k + 4:k + 5].isalnum() or code[k + 4:k + 5] == "_"):
                    j = k + 4
                    continue
                if k < n and code[k] in ";,":
                    return k + 1
                return j + 1
        elif ch in ";," and depth == 0:
            return j + 1
        j += 1
    raise Failure("item without an end at offset %d" % i)


def masked_spans(code):
    """Spans removed from a production build, and `mod x;` names they hide."""
    spans, hidden = [], []
    for m in ATTR.finditer(code):
        close = match_bracket(code, m.end() - 1)
        if cfg_value(code[m.end():close]) is not False:
            continue
        attr_end = code.find("]", close) + 1
        end = item_end(code, attr_end)
        spans.append((m.start(), end))
        decl = re.search(r"\bmod\s+(\w+)\s*;", code[attr_end:end])
        if decl:
            hidden.append(decl.group(1))
    return spans, hidden


def in_spans(offset, spans):
    return any(a <= offset < b for a, b in spans)


# ---------------------------------------------------------------- modules


class Module:
    def __init__(self, rel, modpath, text):
        self.rel = rel
        self.modpath = modpath
        self.text = text
        self.code = blank(text)
        self.masked, self.hidden = masked_spans(self.code)
        self.inline = []  # (start, end, name) of inline `mod name { }`
        for m in re.finditer(r"\bmod\s+(\w+)\s*\{", self.code):
            if in_spans(m.start(), self.masked):
                continue
            brace = m.end() - 1
            self.inline.append((brace, match_bracket(self.code, brace), m.group(1)))

    def target_at(self, offset):
        path = [self.modpath]
        for a, b, name in sorted(self.inline):
            if a < offset < b:
                path.append(name)
        return "::".join(path)


def child_file(parent_rel, name):
    base = os.path.dirname(parent_rel)
    stem = os.path.splitext(os.path.basename(parent_rel))[0]
    if stem not in ("lib", "main", "mod"):
        base = os.path.join(base, stem)
    for cand in (os.path.join(base, name + ".rs"), os.path.join(base, name, "mod.rs")):
        if os.path.exists(os.path.join(SRC, cand)):
            return os.path.normpath(cand)
    raise Failure("module %s declared in %s has no file" % (name, parent_rel))


def load_modules():
    modules = {}
    skipped = set()
    queue = [("lib.rs", "trawl_server"), ("main.rs", "trawld")]
    while queue:
        rel, modpath = queue.pop()
        with open(os.path.join(SRC, rel), encoding="utf-8") as fh:
            mod = Module(rel, modpath, fh.read())
        modules[rel] = mod
        for name in mod.hidden:
            # A hidden module may sit outside src through #[path]; only a
            # file under src needs accounting for.
            try:
                skipped.add(child_file(rel, name))
            except Failure:
                pass
        for m in re.finditer(r"\bmod\s+(\w+)\s*;", mod.code):
            if in_spans(m.start(), mod.masked):
                continue
            if any(a < m.start() < b for a, b, _ in mod.inline):
                raise Failure("%s:%d: `mod x;` inside an inline module" % (rel, line_of(mod.text, m.start())))
            queue.append((child_file(rel, m.group(1)), modpath + "::" + m.group(1)))
    on_disk = set()
    for dp, _, files in os.walk(SRC):
        for f in files:
            if f.endswith(".rs"):
                on_disk.add(os.path.relpath(os.path.join(dp, f), SRC))
    stray = on_disk - set(modules) - skipped
    if stray:
        raise Failure("files no module declares: %s" % ", ".join(sorted(stray)))
    return modules


# ---------------------------------------------------------------- sites


MACRO = re.compile(r"(?<![\w$])(?:::)?(?:tracing::)?(info|warn|error)!\s*[\(\[\{]")
UNREAD = re.compile(r"(?<![\w$])(?:(?:::)?tracing::)?event!\s*[\(\[\{]|(?<![\w$])(?:::)?log::\w+!")
LEVEL_VAR = re.compile(r"(?<![\w$])(?:::)?tracing::\$(\w+)!\s*[\(\[\{]")
QUIET = re.compile(r"(?<![\w$])(?:::)?(?:tracing::)?(debug|trace)!\s*[\(\[\{]")
MACRO_RULES = re.compile(r"\bmacro_rules!\s*(\w+)\s*\{")


def consts(modules):
    found = {}
    for mod in modules.values():
        for m in re.finditer(r"\bconst\s+(\w+)\s*:\s*&(?:'static\s+)?str\s*=\s*\"", mod.code):
            q = m.end() - 1
            end = mod.code.find('"', q + 1)
            found.setdefault(m.group(1), set()).add(mod.text[q + 1:end])
    return found


def fields(mod, a, b):
    """The macro's top-level arguments as original-text strings."""
    return [(s, e, mod.text[s:e].strip(), mod.code[s:e].strip()) for s, e in split_top(mod.code, a, b)]


def describe(mod, a, b, const_map):
    target = None
    event_type = "-"
    for s, e, raw, code in fields(mod, a, b):
        m = re.match(r"target\s*:\s*(.*)$", raw, re.S)
        if m and not raw.startswith("target ="):
            expr = m.group(1).strip()
            if expr.startswith('"'):
                target = expr.strip('"')
            else:
                name = re.sub(r"\.to_owned\(\)$", "", expr).split("::")[-1]
                vals = const_map.get(name)
                if not vals or len(vals) != 1:
                    raise Failure("%s:%d: cannot resolve target %s" % (mod.rel, line_of(mod.text, s), expr))
                target = next(iter(vals))
            continue
        m = re.match(r"event_type\s*=\s*(.*)$", raw, re.S)
        if m:
            val = m.group(1).strip()
            if re.fullmatch(r'"[^"]*"', val):
                event_type = val.strip('"')
            else:
                event_type = "{" + " ".join(val.lstrip("%?").split()) + "}"
        elif re.fullmatch(r"[%?]?event_type", raw):
            event_type = "{event_type}"
    return target, event_type


def find_sites(modules, const_map):
    sites = []
    for rel, mod in sorted(modules.items()):
        for m in UNREAD.finditer(mod.code):
            if not in_spans(m.start(), mod.masked):
                raise Failure("%s:%d: unsupported logging macro %r" % (rel, line_of(mod.text, m.start()), m.group(0)))
        rules = []
        for m in MACRO_RULES.finditer(mod.code):
            if in_spans(m.start(), mod.masked):
                continue
            brace = m.end() - 1
            end = match_bracket(mod.code, brace)
            body = mod.code[brace:end]
            lv = LEVEL_VAR.search(body)
            if lv:
                op = brace + lv.end() - 1
                rules.append((m.group(1), op, match_bracket(mod.code, op), brace, end))
        for m in MACRO.finditer(mod.code):
            if in_spans(m.start(), mod.masked):
                continue
            if any(rb <= m.start() < re_ for _, _, _, rb, re_ in rules):
                continue
            op = m.end() - 1
            cl = match_bracket(mod.code, op)
            target, et = describe(mod, op + 1, cl, const_map)
            sites.append(Site(mod, m.group(1), m.start(), op + 1, cl, target, et, None))
        for name, op, cl, rb, re_ in rules:
            target, et = describe(mod, op + 1, cl, const_map)
            for inv in re.finditer(r"(?<![\w$])%s!\s*\(\s*(\w+)\s*," % re.escape(name), mod.code):
                if in_spans(inv.start(), mod.masked) or rb <= inv.start() < re_:
                    continue
                level = inv.group(1)
                if level not in LEVELS:
                    continue
                sites.append(Site(mod, level, inv.start(), op + 1, cl, target, et, "%s!" % name))
    return sites


class Site:
    def __init__(self, mod, level, start, a, b, target, event_type, via_macro):
        self.mod = mod
        self.level = level
        self.start = start
        self.a, self.b = a, b
        self.line = line_of(mod.text, start)
        self.target = target or mod.target_at(start)
        self.event_type = event_type
        self.via_macro = via_macro
        self.kind = "site" if via_macro is None else "expand:" + via_macro


def enclosing_fn(mod, offset):
    best = None
    for m in re.finditer(r"\bfn\s+(\w+)", mod.code):
        if m.start() > offset:
            break
        brace = mod.code.find("{", m.end())
        semi = mod.code.find(";", m.end())
        if brace < 0 or (0 <= semi < brace and "{" not in mod.code[m.end():semi]):
            continue
        if brace < offset <= match_bracket(mod.code, brace):
            best = (m.group(1), m.start(), brace)
    return best


class Merged:
    """The INFO+ events inside one helper, as one caller-row template."""

    def __init__(self, inner):
        self.level = "/".join(sorted({s.level for s in inner}, key=LEVELS.index))
        self.target = "/".join(sorted({s.target for s in inner}))
        self.event_type = "/".join(sorted({s.event_type for s in inner}))


def helper_params(mod, fn_start):
    """(name, type) of each parameter of the fn whose `fn` keyword is at fn_start."""
    op = mod.code.find("(", fn_start)
    cl = match_bracket(mod.code, op)
    out = []
    for s, e in split_top(mod.code, op + 1, cl):
        raw = " ".join(mod.text[s:e].split())
        if raw.endswith("self"):
            out.append(("self", raw))
        elif ":" in raw:
            name, typ = raw.split(":", 1)
            out.append((re.sub(r"^mut\s+", "", name.strip()), typ.strip()))
    return out


ERRORISH = re.compile(r"Error|Display|Failure|Fault|Result|Errno|Panic|Died")


def discover_helpers(sites):
    """Functions with an INFO+ event that read a parameter in it, or take
    an error-typed parameter at all (its text may go to DEBUG only)."""
    seen = {}
    for s in sites:
        fn = enclosing_fn(s.mod, s.start)
        if fn is None:
            continue
        name, fn_start, _ = fn
        body = s.mod.code[s.a:s.b]
        used = [
            (p, t)
            for p, t in helper_params(s.mod, fn_start)
            if re.search(r"(?<![\w.])%s\b" % re.escape(p), body) or ERRORISH.search(t)
        ]
        if used:
            key = (s.mod.rel, name)
            seen.setdefault(key, set()).update("%s: %s" % u for u in used)
    return ["%s\tfn %s\t%s" % (rel, name, "; ".join(sorted(v))) for (rel, name), v in sorted(seen.items())]


class Call:
    def __init__(self, mod, helper, start, a, b, site):
        self.mod = mod
        self.helper = helper
        self.start = start
        self.a, self.b = a, b
        self.line = line_of(mod.text, start)
        self.site = site
        self.level = site.level
        self.target = site.target
        self.event_type = site.event_type
        self.kind = "call:" + helper
        if "/" in site.level:
            # A helper with events at several levels: the call picks one
            # when it passes a level-named variant (`Loudness::Warn`).
            picked = {m.lower() for m in re.findall(r"\b(?:Loudness|Level|Emit)::(Info|Warn|Error)\b", mod.text[a:b])}
            if len(picked) == 1:
                self.level = picked.pop()
        if site.event_type.startswith("{"):
            # The helper takes event_type from an argument: name the literal
            # the caller passes, if it passes one.
            lits = re.findall(r'"([a-z0-9_]+)"', mod.text[a:b])
            evt = [x for x in lits if "_" in x or x.isalpha()]
            if len(evt) == 1:
                self.event_type = evt[0]


def find_calls(modules, sites):
    calls = []
    for def_rel, name, pattern in HELPERS:
        mod = modules.get(def_rel)
        if mod is None:
            raise Failure("helper file %s is not in the module tree" % def_rel)
        defs = [m for m in re.finditer(r"\bfn\s+%s\b" % re.escape(name), mod.code) if not in_spans(m.start(), mod.masked)]
        if len(defs) != 1:
            raise Failure("%s: expected one fn %s, found %d" % (def_rel, name, len(defs)))
        d = defs[0]
        brace = mod.code.find("{", d.end())
        end = match_bracket(mod.code, brace)
        inner = [s for s in sites if s.mod is mod and brace < s.start < end]
        if not inner:
            raise Failure("%s: fn %s holds no INFO+ event" % (def_rel, name))
        site = Merged(inner)
        if pattern == METHOD:
            bare = re.compile(r"(?<![\w$:])(?:\.\s*|Self::)?%s\s*\(" % re.escape(name))
            scope = [(def_rel, mod)]
        elif pattern is None:
            bare = re.compile(r"(?<![\w$:])(?:self\.|Self::)?%s\s*\(" % re.escape(name))
            for rel, cmod in modules.items():
                if rel == def_rel or re.search(r"\bfn\s+%s\b" % re.escape(name), cmod.code):
                    # A file with its own fn of this name calls that one.
                    continue
                for m in re.finditer(r"(?<![\w$])%s\s*\(" % re.escape(name), cmod.code):
                    if not in_spans(m.start(), cmod.masked) and not re.search(r"\bfn\s+$", cmod.code[max(0, m.start() - 8):m.start()]):
                        raise Failure("%s:%d: calls %s, a helper of %s; give it a call pattern" % (rel, line_of(cmod.text, m.start()), name, def_rel))
            scope = [(def_rel, mod)]
        else:
            bare = re.compile(pattern)
            scope = sorted(modules.items())
        for rel, cmod in scope:
            for m in bare.finditer(cmod.code):
                if in_spans(m.start(), cmod.masked):
                    continue
                if re.search(r"\bfn\s+$", cmod.code[max(0, m.start() - 8):m.start()]):
                    continue
                op = m.end() - 1
                calls.append(Call(cmod, name, m.start(), op + 1, match_bracket(cmod.code, op), site))
    return calls


# ---------------------------------------------------------------- join


def keyed(rows):
    """Attach (file, event_type, level, kind, ordinal) keys in line order."""
    counts = {}
    out = []
    for r in sorted(rows, key=lambda r: (r.mod.rel, r.line, r.start)):
        base = (r.mod.rel, r.event_type, r.level, r.kind)
        counts[base] = counts.get(base, 0) + 1
        out.append((base + (str(counts[base]),), r))
    return out


DISPOSITION = re.compile(r"changed|D2b|unchanged: \S.*")


def load_dispositions():
    table = {}
    with open(DISPOSITIONS, encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            cells = line.split("\t")
            if cells == KEY_COLUMNS:
                continue
            if len(cells) != len(KEY_COLUMNS):
                raise Failure("dispositions.tsv:%d: %d cells, expected %d" % (n, len(cells), len(KEY_COLUMNS)))
            key = tuple(cells[:5])
            if any(not c.strip() for c in cells):
                raise Failure("dispositions.tsv:%d: blank cell in %s" % (n, "\t".join(key)))
            if not DISPOSITION.fullmatch(cells[6]):
                raise Failure("dispositions.tsv:%d: disposition %r is not changed, D2b or unchanged: <reason>" % (n, cells[6]))
            if key in table:
                raise Failure("dispositions.tsv:%d: duplicate key %s" % (n, "\t".join(key)))
            table[key] = (cells[5], cells[6])
    return table


def head_sha():
    sha = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
    dirty = subprocess.run(
        ["git", "-C", REPO, "status", "--porcelain", "--", "crates/trawl-server/src"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    return sha + ("+uncommitted-src" if dirty else "")


def render(rows, table, sha):
    missing = [k for k, _ in rows if k not in table]
    extra = sorted(set(table) - {k for k, _ in rows})
    problems = ["no disposition for site %s:%d (key %s)" % (r.mod.rel, r.line, "\t".join(k)) for k, r in rows if k not in table]
    problems += ["disposition without a site: %s" % "\t".join(k) for k in extra]
    if missing or extra:
        raise Failure("\n".join(problems))
    body = []
    for k, r in rows:
        src_type, disp = table[k]
        cells = [r.mod.rel, str(r.line), r.event_type, r.target, r.level.upper(), src_type, disp]
        body.append("\t".join(cells))
    header = [
        "# Every INFO, WARN and ERROR emission in non-test crates/trawl-server/src code,",
        "# plus one row per call of a logging helper that takes an error value from its",
        "# caller (source type names the helper), and one per macro_rules expansion.",
        "# Paths are relative to crates/trawl-server/src. Rows describe commit:",
        "# sha: %s" % sha,
        "# Search command (a help, not the list: it misses shorthand fields, plain string",
        "# fields and dynamic messages):",
        "#   %s" % SEARCH,
        "# Enumerator (the list): python3 -I docs/evidence/issue-412/enumerate.py",
        "# Dispositions are joined from docs/evidence/issue-412/dispositions.tsv.",
        "# rows: %d" % len(body),
    ]
    return "\n".join(header + ["\t".join(COLUMNS)] + body) + "\n"


def search_hits(modules, rows):
    covered = {}
    for _, r in rows:
        covered.setdefault(r.mod.rel, []).append((r.a, r.b))
    for rel, mod in modules.items():
        # DEBUG and TRACE events are never retained under the default filter.
        for m in QUIET.finditer(mod.code):
            op = m.end() - 1
            covered.setdefault(rel, []).append((op, match_bracket(mod.code, op)))
    out = []
    for rel, mod in sorted(modules.items()):
        # `blank` keeps string delimiters and wipes literal contents, so an
        # odd count of quotes before an offset means it is inside a literal.
        quotes = [0]
        for ch in mod.code:
            quotes.append(quotes[-1] + (ch == '"'))
        offset = 0
        for n, line in enumerate(mod.text.split("\n"), 1):
            for m in SEARCH_RE.finditer(line):
                at = offset + m.start()
                wiped = mod.code[at] == " " and mod.text[at] != " "
                # A wiped hit outside a literal is in a comment. One inside a
                # literal is a format string and stays reportable.
                if in_spans(at, mod.masked) or wiped and quotes[at] % 2 == 0:
                    continue
                if any(a <= at < b for a, b in covered.get(rel, [])):
                    continue
                out.append("%s:%d: %s" % (rel, n, line.strip()))
                break
            offset += len(line) + 1
    return out


def main():
    ap = argparse.ArgumentParser()
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--check", action="store_true")
    g.add_argument("--stdout", action="store_true")
    g.add_argument("--hits", action="store_true")
    g.add_argument("--helpers", action="store_true", help="print functions whose event reads a parameter or that take an error")
    g.add_argument("--keys", action="store_true", help="print each row's key and macro body, for writing dispositions")
    args = ap.parse_args()
    try:
        modules = load_modules()
        sites = find_sites(modules, consts(modules))
        if args.helpers:
            for h in discover_helpers(sites):
                print(h)
            return 0
        calls = find_calls(modules, sites)
        rows = keyed(sites + calls)
        if args.keys:
            for k, r in rows:
                body = " ".join(r.mod.text[r.a:r.b].split())
                print("\t".join(k) + "\t" + str(r.line) + "\t" + r.target + "\t" + body)
            return 0
        if args.hits:
            for h in search_hits(modules, rows):
                print(h)
            return 0
        text = render(rows, load_dispositions(), head_sha())
    except Failure as exc:
        print("enumerate.py: %s" % exc, file=sys.stderr)
        return 1
    if args.stdout:
        sys.stdout.write(text)
        return 0
    if args.check:
        with open(OUT, encoding="utf-8") as fh:
            committed = fh.read()
        strip = lambda t: [l for l in t.split("\n") if not l.startswith("# sha: ")]
        if strip(committed) != strip(text):
            import difflib
            sys.stdout.writelines(difflib.unified_diff(
                [l + "\n" for l in strip(committed)], [l + "\n" for l in strip(text)],
                "retained-events.tsv (committed)", "retained-events.tsv (regenerated)"))
            print("enumerate.py: retained-events.tsv is stale", file=sys.stderr)
            return 1
        return 0
    with open(OUT, "w", encoding="utf-8") as fh:
        fh.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
