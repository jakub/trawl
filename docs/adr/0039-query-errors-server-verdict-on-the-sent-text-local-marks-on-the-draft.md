# A query error shows the server's verdict on the text it received; the local parser marks the draft

status: accepted (2026-09-22) — prep ruling record for #233; amended (2026-11-08), see the Amendment; amended twice (2026-11-09): every terminal sink shows sender text through the sanitize rule, and terminal text is measured by grapheme

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

## Amendment (2026-11-09, prep of #397 and #409): every terminal sink shows sender text through the sanitize rule

The 2026-11-08 amendment shows the caret excerpt "with unsafe characters shown as the existing `trawl_core::sanitize` replacement". That sentence covers the excerpt only, and no record sets the rule for any other terminal sink. PR #394 sanitized the sites #384 named. The prep of 2026-11-09 found more that print sender text raw: `trawl query`'s table headers and cells, the `trawl schema` labels and tables, an embedded engine error, and in the TUI the schema detail, the event popup, the error popup, the query editor, the history pane, the palette and `trawl-dashboard`. ratatui drops control characters, but it draws the bidi, zero-width and soft-hyphen characters that `trawl_core::sanitize` names, so ratatui alone does not keep a terminal safe.

**Text that trawl did not write reaches a terminal only through `sanitize_display_text`, or through `fit_cells` or `pad_cells`, which apply it.** Such text is event values, field names, catalog values, server messages, and the query text of other principals. The rule binds every line the CLI writes to stdout or stderr, every table header and cell, and every TUI span, title and cell. It also binds `trawl-dashboard`, which both the TUI and `trawld monitor` draw.

**A table cell keeps its line breaks.** `\r\n` and `\n` stay line breaks inside a cell, and each line is sanitized, so a tab, a lone `\r`, ESC, a C1 control or a bidi character shows as U+FFFD. comfy-table draws the column borders on every line of a cell, so a line break cannot leave its cell. In a prose line and in a TUI span, a line break is U+FFFD, because there it would start a line that the sender wrote.

**Machine formats stay data.** In a pipe and in an `-o` file, `json` and `csv` are byte-exact. On a terminal, `json` writes every unsafe character as a `\uXXXX` escape, which decodes to the same value, and `csv` writes U+FFFD in place of every unsafe character. The human ruled the `csv` case on 2026-11-09: "Replace unsafe characters", on a terminal only.

**Sanitizing changes what is drawn, never what is kept.** The editor buffer, a submitted query, a cached response and a palette action keep the text as it came. `json` and `csv` written to a pipe or a file keep their data as it is today. Table output is a human format, so it is sanitized even when it is written to a file.

The rule stops a sender from moving the cursor, clearing or retitling the terminal, setting its clipboard, and reordering or hiding text. It does not stop printable text that only looks misleading, such as a confusable letter or a printable `│`.

Considered, not adopted:

- **A sanitizing writer around stdout and stderr.** It cannot tell a table from `json` data on the same stream, and it cannot serve the TUI, where crossterm must write escapes.
- **Refusing `csv` on a terminal.** The human chose replacement. A refusal keeps every value exact but sends the operator to a pipe for a quick look.
- **One-line table cells.** A stack trace in one cell would become one long line, and the borders already contain a line break.
- **A lint that bans `Span::raw` and `Line::from(String)`.** Fixed labels use those constructors everywhere, so the ban would need hundreds of exceptions and would still miss other conversions.

## Amendment (2026-11-09, prep of #398): terminal text is measured by grapheme, as ratatui draws it

The 2026-11-08 amendment measures carets "with `unicode-width` on the kept string", the whole-string measure in `trawl_core::text_layout`. On 2026-11-09, during #384's run, the human kept that measure and declined a segmentation dependency, and #398 recorded the gap. The prep of the same day reopened it, and the human ruled "Count by grapheme" on 2026-11-09. That ruling reverses the earlier one, for three reasons:

- **The old measure can draw at twice its budget.** unicode-width counts lam-alef (`لا`) as one cell, and ratatui draws each grapheme, two cells. Nine pairs measure 9 cells and draw 18, so `fit_cells` returns text that overruns its column.
- **The proposed bound is not a bound.** The larger of the whole-string width and the per-character sum gives 3 cells for `❤\u{FE0F}لا`, which draws in 4.
- **The dependency adds no crate.** `unicode-segmentation` is an unconditional dependency of chumsky 0.13, which trawl-core already uses, so a direct dependency changes no build graph, the wasm and fuzz builds included.

**The CLI and the TUI measure sanitized text per extended grapheme cluster.** Each cluster takes its unicode-width, plus one cell for each U+FF9E or U+FF9F. That is the cell width ratatui-core 0.1.2 draws. A cut falls between clusters. A span whose end lies inside a cluster marks every cell of that cluster. The rule that decides whether a span is valid does not change.

The sentence "The terminal clients and `trawl-mcp` place the caret by display cells, measured with `unicode-width` on the kept string" now reads: the CLI and the TUI place the caret by display cells, measured per grapheme cluster on the kept string. `trawl-mcp` keeps its own measure, its byte window and its tab handling (`crates/trawl-mcp/src/render/refusal.rs`). The measure describes the pinned ratatui model, not the glyph shaping of every terminal and font.

Considered, not adopted:

- **Keep the whole-string measure and fix only the sites that count bytes.** The overrun above stays.
- **The larger of the two cheap measures as an upper bound.** It is not one.
