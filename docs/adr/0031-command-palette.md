# The command palette is Shell's own trapped combobox over real anchors

status: accepted (2026-09-08) — prep ruling record for #100 (slice G4)

ADR-0025 ruled the ⌘K stub becomes a real palette in fleet-ui with a
routes-only command set — the consumer's mode tabs and rail items, the
data `Shell` already receives — and ADR-0028 left the stub untouched for
this slice. Both design legs of the prep dialectic converged after
critique on the shape below; the three contested forks (activation,
chord posture, chord-while-open) were settled by enumerating the actual
command inventory at HEAD and by human ruling.

## Decision

**Shell derives the commands and owns the palette; consumers pass
nothing new.** A crate-private `command_palette` module normalizes
`ModeTab` + `RailItem` into commands (mode order then rail order, deduped
by exact path, mode wins), mounted by `Shell` under its own open signal.
`TopBar` gains two REQUIRED props (`on_open_palette`, `palette_open`) —
optional ones would permit a dead trigger, the exact ADR-0025 sin.

**A `Trap` overlay dialog, combobox-over-listbox, focus pinned to the
input** (aria-activedescendant, `roving::next_index` for the walk; Home/
End stay caret keys). Headerless — input first, named by `aria-label`
"Command palette" (human ruling over a visible heading) — with a named
native close button (fleet's own dialog language and the only touch
dismissal) and a visually-hidden polite status region announcing count /
no-matches. Options are grouped "Modes" / "Sections" and every row shows
its path as dim secondary text: the label "Schema" appears twice at
HEAD, so the path is disambiguation, not polish. Filtering is trimmed,
case-insensitive, every-token-substring over label OR path. Reopen
resets the filter; a row navigable to the current location renders no
"current" affix unless the paths are exactly equal.

**Activation is a real router anchor, not a callback.** Every command in
the ruled set is a bare app-authored static path — the exact hrefs the
rail and mode tabs already render as unguarded `<A>`s — and the refusing
navigator guards user-authored query links, which the palette does not
construct. Options are `<A role="option" tabindex="-1">`; Enter clicks
the active anchor; modified clicks (cmd/middle) keep native new-tab
behaviour. When a later prep admits refusable commands (actions, saved
queries, search), THAT prep owns the refusal lane.

**Chord posture.** `Meta+K` and `Ctrl+K` both open it (exactly one of
meta/ctrl, no alt/shift, ignoring repeat, `defaultPrevented` and IME
composition, listened at window bubble phase in Shell — the crate's
first always-on key listener). The one carve-out: on macOS, `Ctrl+K`
with an editable event target is left to the editor (Cocoa kill-line —
a real competing editing command, unlike Firefox's page-preemptable
Ctrl+K; the blanket alternative would break both chords in the DSL
editor, the app's most common focus state). The chord is inert while any
other overlay is registered, and toggles the palette closed when it is
already open (human ruling; symmetric open/dismiss). The trigger becomes
a `<button>` keeping the ghost-box look ("Go to…" replacing the lying
"Search…"), its glyph chip driven by one pure UA helper that also feeds
the macOS carve-out, rendering the platform-primary chord only.

## Consequences

- Empty command inventory ⇒ no trigger and no chord handling (a control
  with nothing behind it is the ADR-0025 ban).
- The palette mirrors the mode tabs and active rail items. Health has
  its own `/settings/health` destination after G3. G1 removes the remaining
  placeholder entries and points Schema at `/search/schema`. Its final
  Settings-mode commands are Search, Jobs, Settings, Health and Schema.
  Settings keeps `/settings`, so exact-path deduplication preserves the
  separately named Health command. Help stays outside this inventory in
  the rail's bottom slot.
- `component_class_contract.rs`'s stub assertion passes vacuously after
  this change (the title string survives in the trigger); the replacement
  pins are written deliberately: `<button`, `aria-haspopup="dialog"`,
  combobox/listbox roles, `FocusPolicy::Trap`, `is_composing`, and the
  absence of "coming soon".
- Automated proof stays Chromium-only per ADR-0028's standing budget;
  browser-chrome preemption (Firefox Ctrl+K, Safari Meta+K) gets a
  one-time manual headed check recorded under `visual-evidence/` and is
  otherwise a declared residual.
- Coastwatch receives the working palette on its next `TRAWL_REV` bump;
  the two required `TopBar` props are the compile-visible break the bump
  PR names.
