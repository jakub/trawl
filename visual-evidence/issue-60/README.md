# issue #60 — the namespace cutover (ADR-0013 slice 1)

## `tui-severity-tokens-transcript.txt`

The TUI results table under the reshaped envelope. Read it for three
things at once:

- `_severity` DISPLAYS its OTel token — `17` reads `error`, `18` reads
  `error2` — so a cell reads as exactly the value that would filter it.
  The rendering is INJECTIVE: `error2` is not shown as `error`.
- `level` sits BESIDE it, verbatim, as ordinary sender vocabulary. The
  derivation observed it; it never consumed it.
- the game-server row (`level: "gold"`, no severity source that maps)
  carries an empty `_severity` and keeps its own column — the #60
  canonical example, on screen.

Regenerate: `cargo test -p trawl-cli --lib render_with_severity_tokens`
(the transcript IS the insta snapshot, so a rendering change fails the
test rather than drifting silently).

## `search.severity-only.png`

The SPA's half of the same contract, in the browser — the real
`trunk build` wasm against a stub backend serving one hand-built
`QueryResponse` (no dev stack, so no fixed ports and no shared-worktree
state). Three columns sit side by side on purpose:

- `_severity` renders the OTel token in a band-coloured pill — `error`,
  `error2`, `warn`, `info`, `debug`, `fatal` — and the `game`/`gold` row
  shows the empty slot, uncoloured.
- `severity` beside it is ordinary sender data: the numbers render
  verbatim, uncoloured. So does `level`.
- the `legacy` row is the discriminator: bare `severity` is `17` while
  `_severity` is `9`. It renders `INFO` and its histogram bar is BLUE.
  A `severity_text`/bare-`severity` fallback would have painted it red.

The histogram's three red bars are exactly the three rows whose
`_severity` is at or above 17.

`crates/trawl-web-ui/src/severity_cell.rs` is where those decisions
live, as pure functions with a native test table — including
`severity_column`, the one lookup both call sites use, so "reads
`_severity` only" is decided in natively-tested code rather than twice
inside wasm-gated components. `cargo check -p trawl-web-ui --target
wasm32-unknown-unknown` (the CI gate, `RUSTFLAGS=-D warnings`) is what
proves those call sites compile at all.
