// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native drift guard for issue #118's Playwright suite (`e2e/`).
//!
//! `e2e/selectors.ts` is the single source every spec imports selector
//! strings and expected copy from. This test proves both halves of that
//! contract hold: the literal actually appears in `selectors.ts` (a typo
//! there would silently break every spec importing it), AND the "hook"
//! substring the selector is built from still appears in the Rust (or
//! fleet-ui) source file that is supposed to emit it — the same
//! `include_str!`-and-scan idiom as
//! `crates/fleet-ui/tests/component_class_contract.rs`, applied across
//! the crate boundary into the e2e suite's own selector sheet instead of
//! into a stylesheet.
//!
//! This cannot run the SPA or `CodeMirror`, so it is not a substitute for
//! actually running the suite (`cargo xtask e2e`) — it only proves the
//! selectors/copy the suite is BUILT from haven't silently drifted out
//! from under it between runs.

const SELECTORS_TS: &str = include_str!("../e2e/selectors.ts");

const EDITOR_RS: &str = include_str!("../src/components/editor.rs");
const HISTORY_RS: &str = include_str!("../src/pages/history.rs");
const LAYOUT_RS: &str = include_str!("../src/pages/layout.rs");
const EDITOR_WRAP_RS: &str = include_str!("../src/components/editor_wrap.rs");
const LOADED_COMPONENT_RS: &str = include_str!("../../fleet-ui/src/loaded/component.rs");
const LOADED_STATE_RS: &str = include_str!("../../fleet-ui/src/loaded/state.rs");
const RAIL_RS: &str = include_str!("../../fleet-ui/src/rail.rs");
const SECTION_RS: &str = include_str!("../src/state/section.rs");

/// One (literal, source file, hook) triple: `literal` must appear
/// verbatim in `selectors.ts`, and `hook` must appear in `source`.
struct Contract {
    literal: &'static str,
    source_path: &'static str,
    source: &'static str,
    hook: &'static str,
}

const CONTRACTS: &[Contract] = &[
    Contract {
        literal: ".dsl-editor",
        source_path: "src/components/editor.rs",
        source: EDITOR_RS,
        hook: "class=\"dsl-editor\"",
    },
    Contract {
        literal: "Search history",
        source_path: "src/pages/history.rs",
        source: HISTORY_RS,
        hook: "\"Search history\"",
    },
    Contract {
        literal: "That page does not exist.",
        source_path: "src/pages/layout.rs",
        source: LAYOUT_RS,
        hook: "That page does not exist.",
    },
    Contract {
        literal: "button.run",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"run\"",
    },
    Contract {
        literal: "dr-trigger",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"dr-trigger\"",
    },
    Contract {
        literal: "Real-time",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Real-time\"",
    },
    Contract {
        literal: "Live Tail",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "\"Live Tail\"",
    },
    Contract {
        literal: "rt-hint",
        source_path: "src/components/editor_wrap.rs",
        source: EDITOR_WRAP_RS,
        hook: "class=\"rt-hint\"",
    },
    Contract {
        literal: ".load-hint.error",
        source_path: "../fleet-ui/src/loaded/component.rs",
        source: LOADED_COMPONENT_RS,
        hook: "class=\"load-hint error\"",
    },
    Contract {
        literal: "Couldn't load",
        source_path: "../fleet-ui/src/loaded/state.rs",
        source: LOADED_STATE_RS,
        hook: "Couldn't load {what}: {msg}",
    },
    Contract {
        literal: "nav.rail",
        source_path: "../fleet-ui/src/rail.rs",
        source: RAIL_RS,
        hook: "class=\"rail\"",
    },
    Contract {
        literal: "History",
        source_path: "src/state/section.rs",
        source: SECTION_RS,
        hook: "\"History\"",
    },
];

#[test]
fn every_selector_literal_appears_in_selectors_ts() {
    for c in CONTRACTS {
        assert!(
            SELECTORS_TS.contains(c.literal),
            "e2e/selectors.ts no longer contains `{}` — a spec importing \
             SEL/COPY from it would break at runtime. It was pinned \
             against {}'s `{}` hook; re-verify both sides and update \
             selectors.ts, or this contract's literal, together.",
            c.literal,
            c.source_path,
            c.hook,
        );
    }
}

#[test]
fn every_source_hook_still_exists() {
    for c in CONTRACTS {
        assert!(
            c.source.contains(c.hook),
            "{} no longer contains the hook `{}` that e2e/selectors.ts's \
             `{}` selector/copy is built from — the Playwright suite \
             would then be asserting against markup that doesn't exist. \
             Update selectors.ts (and the specs that use it) to match \
             the real source, or restore the hook if the change was \
             accidental.",
            c.source_path,
            c.hook,
            c.literal,
        );
    }
}
