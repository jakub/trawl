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

## web

The SPA's half of the same contract is machine-checked rather than
screenshotted: `crates/trawl-web-ui/src/severity_cell.rs` holds the
display text, the band class and the histogram's error predicate as pure
functions with a native test table, and `components/results_table.rs` /
`components/histogram.rs` are thin call sites over it. A live screenshot
was not captured in this run — the dev stack binds fixed ports and this
worktree is shared — so that evidence item is outstanding.
