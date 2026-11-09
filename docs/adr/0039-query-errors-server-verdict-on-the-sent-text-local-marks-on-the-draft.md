# A query error shows the server's verdict on the text it received; the local parser marks the draft

status: accepted (2026-09-22) — prep ruling record for #233; amended (2026-11-08), see the Amendment

The Search page sends the server the effective query, which is the
executed query with the structured state merged in: filter clauses and
the range are prepended, a pipeline-only draft gains a `*` search stage,
and whitespace is rebuilt (`crates/trawl-search-url/src/query_merge.rs`).
The server parses that text and answers a parse or validation failure
with the documented envelope: a code, a message, and details that may
carry a byte span into the text it received. Those spans index text the
editor never shows, so they cannot be drawn on the editor buffer as
they are.

The browser also runs the same trawl-core parser, compiled to wasm, on
every keystroke of the draft, and feeds CodeMirror's lint gutter from
it. ADR-0014 listed "the web UI's CodeMirror squiggles" among the
consumers of the wire `ErrorSpan`. That was never true, and this record
says it will not be.

## Decision

Two owners, never crossed.

The **server** owns whether a query ran and why not. Its envelope is
kept whole in the browser. A `parse_error` or `validation_error` is a
query error: it renders in the results region, on both results tabs, as
a notice that quotes the server's message and, for every detail with a
span, an excerpt of the query as sent with a caret under the span. The
excerpt is labelled as the query sent to the server. A query error
offers no Retry: the text will fail again. Every other failure keeps
today's copy and its Retry.

The **local parser** owns the marks in the draft: the gutter marker and
a visible draft diagnostic under the editor. It never gates a
submission, never sources the results notice, and its spans are never
compared with the server's. When the two disagree, the server's verdict
stands on ran-or-refused and the local marks stand on the draft; nothing
reconciles them, because a disagreement is a version skew the user
should see, not a state a UI can hide.

A failed request carries the effective query it was for, the same way a
successful one does, so the excerpt always quotes the text that failed
and never the draft that replaced it.

Live mode keeps the browser's `EventSource`. The stream handler rejects
the DSL before the SSE response exists, so a refused live query closes
the socket before it opens and the browser sees only that. When that
happens and the local parser also rejects the exact text that was sent,
the page shows the same query error notice, worded so that each sentence
is true on its own: the stream could not start, and the query has a
syntax error. Otherwise the live failure copy is unchanged. A live
validation error therefore stays generic; that is a known limit of the
transport, recorded here rather than worked around.

## Considered and rejected

- **Mapping server spans back into the draft** by keeping provenance
  through the merge. It is a second implementation of the merge that
  must agree with the first forever, and it has no target when the error
  sits in a generated clause. The excerpt is exact by construction.
- **Replacing `EventSource` with a fetch-based SSE reader** so live mode
  can read the 400 body. It re-implements reconnect, backoff, chunk
  decoding and cancellation that the browser provides, for one message.
- **A `/api/v1/validate` preflight before opening the stream.** It needs
  a permission the session key may not hold and adds a race between the
  check and the open.
- **Whole-query spans for validation errors.** The emitter's span helper
  covers the entire input; a caret under everything is noise. The
  server's validation details carry the message and hint with no span.

## Amendment (2026-11-08): one rule decides whether a span is valid, and every client quotes the submitted text

This amendment records the untangle prep of 2026-11-08 (defects D2 and D18). The decision above is about the browser. The terminal clients drew the same spans by other rules:

- `trawl query`'s `render_span_error` clamps a bad span to the text's length, despite its "Clamp to valid byte boundaries" comment, and uses byte counts as screen columns (`crates/trawl-cli/src/cli.rs:553-584`). The TUI error pane does the same, and it slices `query_line[..col_start]` at an unchecked offset, which panics inside a multibyte character (`crates/trawl-cli/src/tui/ui/results.rs:680-683`).
- The TUI draws the server's spans over the live editor buffer (`results.rs:635`, `tab.editor.text()`). `QueryError` keeps no submitted text (`crates/trawl-cli/src/tui/mod.rs:67-73`), so an edit between submit and reply moves the caret onto other text.
- `trawl-mcp`'s `excerpt` (`crates/trawl-mcp/src/render/refusal.rs:320`) and the web UI's `locate` (`crates/trawl-web-ui/src/query_error.rs:58`) already refuse a malformed span and strip CRLF's CR, each with its own copy of the rule.

**A span is a pair of UTF-8 byte offsets into the submitted text, and one locator in trawl-core decides whether it is valid.** It takes raw offsets, not `trawl-api`'s `ErrorSpan`, so the kit stays below its consumers. A span is invalid when it is reversed, ends past the text, or has an endpoint that is not on a character boundary. A client shows an invalid span's message and hint with no excerpt. It never clamps a span into a different one. For a valid span the locator returns the line that holds the start: `\r\n` is one line ending, a span that runs onto a later line is clipped to the start's line, and a zero-width span or one at the end of input gets one caret.

**The excerpt quotes the submitted text in every client.** The sentence above, "the excerpt always quotes the text that failed and never the draft that replaced it", now binds the terminal clients too. The TUI keeps the submitted query with its `QueryError` and locates spans in that text.

**Layout stays with each client.** The terminal clients and `trawl-mcp` place the caret by display cells, measured with `unicode-width` on the kept string, with unsafe characters shown as the existing `trawl_core::sanitize` replacement. `trawl-mcp` keeps its byte-bounded context and ADR-0058's 8 KiB error cap. The browser keeps its hidden-prefix caret padding and character columns. Spans on the wire are unchanged.

Considered, not adopted:

- **Record nothing, because the TUI fix only conforms to the sentence above.** The sentence was written for the browser, and nothing said the terminal clients were bound by it or that a bad span must not be clamped. Two terminal renderers drifted for exactly that reason.
- **Convert spans to character offsets on the wire.** Every consumer already holds the text, and a byte offset is what the parser produces. Changing the wire unit would touch every client for no gain.
- **One shared caret renderer for the terminal and the browser.** Terminal spaces cannot replace the browser's hidden prefix, which keeps wide and combining glyphs placed by the browser's own layout.
