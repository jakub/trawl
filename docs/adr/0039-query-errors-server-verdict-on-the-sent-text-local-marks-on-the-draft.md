# A query error shows the server's verdict on the text it received; the local parser marks the draft

status: accepted (2026-09-22) — prep ruling record for #233

The Search page sends the server the effective query, which is the
executed query with the structured state merged in: filter clauses and
the range are prepended, a pipeline-only draft gains a `*` search stage,
and whitespace is rebuilt (`crates/trawl-web-ui/src/query_merge.rs`).
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
