// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Closed icon enum + renderer. Single SVG style: 16×16 viewBox,
//! `currentColor` stroke, 1.4–1.5 stroke-width. Apps override visual
//! size via the `size` prop on `IconView` (CSS doesn't size these
//! because the consumer expects the `width`/`height` attributes to be
//! present for layout).
//!
//! Adding a 17th glyph is a one-line enum change plus a match arm. No
//! escape hatch by design — ADR-0030 wants the fleet to share visual
//! DNA, not let consumers smuggle arbitrary SVGs into the design system.

use leptos::prelude::*;

/// Every icon shipped by fleet-ui. Used by the rail, the topbar, and
/// the confirm modal. Variant order: shared chrome first, then rail
/// glyphs in the original order they appeared in trawl-web-ui's
/// `RailIcon` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    // shared chrome
    Search,
    Chevron,
    Bell,
    Close,
    // rail
    Clock,
    Database,
    News,
    Alert,
    Link,
    Zap,
    Check,
    Grid,
    User,
    Chart,
    Question,
}

/// Renders an [`Icon`] as a 16×16 inline SVG. `stroke_width` defaults
/// to 1.4 (rail-tuned); `size` defaults to 16 so the SVG matches the
/// viewBox 1:1 unless the caller wants something smaller (topbar uses
/// 10–14, modal close uses 12).
#[component]
pub fn IconView(
    icon: Icon,
    #[prop(default = 16)] size: u16,
    #[prop(default = 1.4)] stroke_width: f32,
) -> impl IntoView {
    let body = icon_body(icon);
    view! {
        <svg
            width=size
            height=size
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width=stroke_width
        >
            {body}
        </svg>
    }
}

fn icon_body(icon: Icon) -> AnyView {
    match icon {
        Icon::Search => view! {
            <g><circle cx="7" cy="7" r="4.5"/><path d="m10.5 10.5 3 3"/></g>
        }
        .into_any(),
        Icon::Chevron => view! {
            <g><path d="m4 6 4 4 4-4"/></g>
        }
        .into_any(),
        Icon::Bell => view! {
            <g>
                <path d="M3.5 12V7a4.5 4.5 0 1 1 9 0v5l1 1.5h-11l1-1.5z"/>
                <path d="M7 14a1 1 0 0 0 2 0"/>
            </g>
        }
        .into_any(),
        Icon::Close => view! {
            <g><path d="m4 4 8 8M12 4l-8 8"/></g>
        }
        .into_any(),
        Icon::Clock => view! {
            <g><circle cx="8" cy="8" r="6"/><path d="M8 5v3l2 1.5"/></g>
        }
        .into_any(),
        Icon::Database => view! {
            <g>
                <ellipse cx="8" cy="3.5" rx="5" ry="1.5"/>
                <path d="M3 3.5v9c0 .8 2.2 1.5 5 1.5s5-.7 5-1.5v-9"/>
                <path d="M3 8c0 .8 2.2 1.5 5 1.5s5-.7 5-1.5"/>
            </g>
        }
        .into_any(),
        Icon::News => view! {
            <g>
                <rect x="2" y="3" width="11" height="10" rx="1"/>
                <path d="M4.5 6h6M4.5 8.5h6M4.5 11h4"/>
            </g>
        }
        .into_any(),
        Icon::Alert => view! {
            <g>
                <path d="M8 2 14 13H2z"/>
                <path d="M8 6.5v3M8 11.5v.01" stroke-linecap="round"/>
            </g>
        }
        .into_any(),
        Icon::Link => view! {
            <g>
                <path d="M9 4.5h2.5a2.5 2.5 0 0 1 0 5H9"/>
                <path d="M7 11.5H4.5a2.5 2.5 0 0 1 0-5H7"/>
                <path d="M5.5 8h5"/>
            </g>
        }
        .into_any(),
        Icon::Zap => view! {
            <g><path d="M9 1.5 3.5 9.5h4l-1 5L12 6.5h-4z"/></g>
        }
        .into_any(),
        Icon::Check => view! {
            <g><path d="m3 8.5 3.5 3L13 4.5"/></g>
        }
        .into_any(),
        Icon::Grid => view! {
            <g>
                <rect x="2" y="2" width="5" height="5"/>
                <rect x="9" y="2" width="5" height="5"/>
                <rect x="2" y="9" width="5" height="5"/>
                <rect x="9" y="9" width="5" height="5"/>
            </g>
        }
        .into_any(),
        Icon::User => view! {
            <g>
                <circle cx="8" cy="5.5" r="2.5"/>
                <path d="M3 14c0-2.8 2.2-5 5-5s5 2.2 5 5"/>
            </g>
        }
        .into_any(),
        Icon::Chart => view! {
            <g><path d="M2 13.5h12M4 11V7.5M7 11V4.5M10 11V8.5M13 11V6"/></g>
        }
        .into_any(),
        Icon::Question => view! {
            <g>
                <circle cx="8" cy="8" r="6"/>
                <path d="M6 6.5c0-1.1.9-2 2-2s2 .9 2 2c0 1.5-2 1.5-2 3"/>
                <path d="M8 11.5v.01" stroke-linecap="round"/>
            </g>
        }
        .into_any(),
    }
}
