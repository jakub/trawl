// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Native contract test for the issue #33 D5 story-404 papercut fix.
//!
//! D5's named evidence is a before/after screenshot of the story 404
//! page in a PR comment — but `trawl-web-ui` compiles leptos only under
//! `cfg(target_arch = "wasm32")`, so no `cargo nextest` run can *render*
//! `StoryPage` and observe the pixels. The screenshot stays the manual
//! PR-time deliverable for the truly-rendered surface; what a native
//! test *can* do — the same `include_str!`-and-scan trick
//! `css_move_invariant` and fleet-ui's `component_class_contract` use —
//! is read the call site and pin the wiring the screenshot would show,
//! so the papercut fix regresses under CI, not just under a human's eye.
//!
//! The fleet-ui side (the `Missing` arm renders the neutral `.load-hint`
//! with a `.load-sub` subtitle, never the red `.load-hint.error`) is
//! pinned by `fleet-ui/tests/component_class_contract.rs`; the
//! 404-to-`Missing` mapping by `loaded::state`'s unit tests. This
//! guards the third,
//! app-side leg fleet-ui cannot see: that trawl's story page actually
//! *feeds* those primitives a 404-classified `Missing` state carrying
//! the restored explanatory subtitle — and no longer the doubled
//! "couldn't load story: story not found" copy the pre-fix shape emitted.

const STORY: &str = include_str!("../src/pages/intel/story.rs");

#[test]
fn story_404_is_a_missing_state_not_an_error() {
    // The classifier form is what splits "the story doesn't exist" off
    // from real failures; `from_resource_with` (no `_missing`) is the
    // pre-fix shape that error-copied every status, including 404.
    assert!(
        STORY.contains("from_resource_with_missing("),
        "the story page must classify via LoadState::from_resource_with_missing \
         so a 404 becomes LoadState::Missing (neutral), not an Error"
    );
    assert!(
        STORY.contains("matches!(e, ApiError::Status(404))"),
        "404 must be the missing-classifier's predicate — a missing story \
         renders the neutral \"story not found\" hint, not a failure"
    );
    // The pre-fix papercut: 404 mapped to an error *string*, which
    // `<Loaded>`'s label form then doubled into "couldn't load story:
    // story not found". That arm must be gone.
    assert!(
        !STORY.contains(r#"ApiError::Status(404) => "story not found""#),
        "404 must no longer map to an error string — that produced the \
         doubled \"couldn't load story: story not found\" copy the D5 fix \
         removes"
    );
}

#[test]
fn story_missing_restores_the_explanatory_subtitle() {
    // The exact explanatory line the slice-C migration dropped and D5
    // restores, recovered from pre-slice-C history. Its presence is the
    // whole visible payload of the "after" screenshot.
    assert!(
        STORY.contains(
            "missing_subtitle=\"The story may not exist or you may not \
             have permission to view it.\""
        ),
        "the story page must pass the restored explanatory subtitle to \
         <Loaded>'s missing_subtitle slot (the slice-C papercut D5 fixes)"
    );
    // `label="story"` is what makes the canonical missing copy read
    // "story not found" rather than the bare "not found".
    assert!(
        STORY.contains(r#"label="story""#),
        "the story page must name the resource via label=\"story\" so the \
         canonical missing copy reads \"story not found\""
    );
}

#[test]
fn story_503_keeps_its_error_copy() {
    // The other half of the split: non-404 failures stay Errors with
    // their bespoke copy, routed through the classifier's render_err arm.
    assert!(
        STORY.contains(r#"ApiError::Status(503) => "intel service unavailable""#),
        "503 must keep its error copy through the render_err arm — only \
         404 is reclassified as Missing"
    );
}
