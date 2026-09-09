// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Leptos 0.8 design system for fleet apps (ADR-0030).
//!
//! Ships design tokens, reset + base styles, the theme preference
//! system, and the typed component surface, by category:
//!
//! - **chrome** — `Shell`, `TopBar`, `Rail`, `Login`, `Toasts`;
//! - **overlays** — the `Modal` family, `Drawer`, `ActionsMenu`, all
//!   arbitrated by the [`overlay`] stack (topmost-only Escape + focus
//!   ownership);
//! - **form & actions** — `Btn`, `Field`, `Toggle`, `Segmented`,
//!   `SearchInput`, `CopyButton`, `Kbd`;
//! - **content & data** — `Badge`, `StatusDot`, `Sparkline`, `Tabs`,
//!   `Loaded`, `Pager`, `LoadMore`, `When`, `ErrorBanner`, `IconView`;
//! - **decoration** — `Atmosphere`, the WebGL mesh-gradient backdrop
//!   over the vendored `@paper-design/shaders` bundle, which rides
//!   along as a wasm-bindgen snippet — no consumer build wiring, but
//!   behind the default-off `atmosphere` cargo feature, because
//!   linking the snippet plants its 142 KB in the dist of every
//!   consumer, mounted or not (ADR-0012, [`atmosphere`]).
//!
//! Consumed by trawl-web-ui and coastwatch via path deps; CSS is
//! consumed via Trunk's `data-trunk rel="css"` directive pointing at
//! `styles/fleet-ui.css` in this crate's directory.
//!
//! Most modules are wasm32-only — they pull leptos / web-sys / gloo
//! and only make sense in a browser. The exceptions are the pure
//! layers that build on every target — [`theme::prefs`],
//! [`toast::kinds`], [`toast::stack`], [`button::variant`],
//! [`login::validate`], [`badge::tone`], [`status_dot::tone`],
//! [`sparkline::geometry`], [`loaded::state`], [`load_more`]'s phase
//! resolution, [`copy_button`]'s toast decision,
//! [`modal::confirm_state`], the [`overlay`] stack, the [`time`]
//! formatters, [`atmosphere::palette`] (the shader knobs site, its
//! stops parity-pinned against the stylesheet), and the [`icon::Icon`]
//! enum — so their contracts (localStorage JSON, CSS-class
//! composition, state machines, canonical copy, focus ownership,
//! timestamp buckets) are exercised by native unit tests and nameable
//! by native consumer code. The renderers themselves stay wasm32-only.

pub mod atmosphere;
pub mod badge;
pub mod button;
// The route palette stays crate-private; its pure rules also compile in native tests.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod command_palette;
pub mod copy_button;
pub mod load_more;
pub mod loaded;
pub mod login;
pub mod modal;
pub mod overlay;
pub mod page_window;
pub mod range_dialog;
pub mod segmented;
pub mod sparkline;
pub mod status_dot;
pub mod theme;
pub mod time;
pub mod toast;

#[cfg(target_arch = "wasm32")]
pub mod actions_menu;
#[cfg(target_arch = "wasm32")]
pub mod clipboard;
#[cfg(target_arch = "wasm32")]
pub mod drawer;
#[cfg(target_arch = "wasm32")]
pub mod error_banner;
#[cfg(target_arch = "wasm32")]
pub mod field;
pub mod icon;
#[cfg(target_arch = "wasm32")]
pub mod kbd;
// The shared menu contract is crate-internal: both menus are the public
// surface, the panel is not. Compiled on native test builds too, so its
// pure half (restore-by-cause) is a `nextest` fact.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod menu;
#[cfg(target_arch = "wasm32")]
pub mod pager;
#[cfg(target_arch = "wasm32")]
pub mod rail;
// Roving-tabindex index arithmetic, crate-internal and native-tested
// for the same reason.
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod roving;
#[cfg(target_arch = "wasm32")]
pub mod search_input;
#[cfg(target_arch = "wasm32")]
pub mod shell;
#[cfg(target_arch = "wasm32")]
pub mod tabs;
#[cfg(target_arch = "wasm32")]
pub mod toggle;
#[cfg(target_arch = "wasm32")]
pub mod topbar;

pub use badge::Tone;
pub use button::{Size, Variant};
pub use icon::Icon;
pub use load_more::LoadMorePhase;
pub use loaded::LoadState;
pub use modal::ConfirmState;
#[cfg(target_arch = "wasm32")]
pub use page_window::OffsetPager;
pub use page_window::{PageTotal, PageWindow};
#[cfg(target_arch = "wasm32")]
pub use range_dialog::RangeDialog;
pub use range_dialog::{RangePreset, RangeValue};
pub use segmented::{SegmentedOption, segmented_class};
pub use sparkline::SparkPath;
pub use status_dot::StatusTone;
pub use theme::{RowStyle, Theme};
pub use toast::{Toast, ToastKind, ToastStack};

#[cfg(target_arch = "wasm32")]
pub use actions_menu::{ActionItem, ActionsMenu};
#[cfg(all(target_arch = "wasm32", feature = "atmosphere"))]
pub use atmosphere::Atmosphere;
#[cfg(target_arch = "wasm32")]
pub use badge::Badge;
#[cfg(target_arch = "wasm32")]
pub use button::Btn;
#[cfg(target_arch = "wasm32")]
pub use clipboard::write_clipboard;
#[cfg(target_arch = "wasm32")]
pub use copy_button::CopyButton;
#[cfg(target_arch = "wasm32")]
pub use drawer::Drawer;
#[cfg(target_arch = "wasm32")]
pub use error_banner::ErrorBanner;
#[cfg(target_arch = "wasm32")]
pub use field::{Field, Helper};
#[cfg(target_arch = "wasm32")]
pub use icon::IconView;
#[cfg(target_arch = "wasm32")]
pub use kbd::Kbd;
#[cfg(target_arch = "wasm32")]
pub use load_more::LoadMore;
#[cfg(target_arch = "wasm32")]
pub use loaded::Loaded;
#[cfg(target_arch = "wasm32")]
pub use login::Login;
#[cfg(target_arch = "wasm32")]
pub use modal::{ConfirmModal, ConfirmWithReasonModal, Modal};
#[cfg(target_arch = "wasm32")]
pub use pager::Pager;
#[cfg(target_arch = "wasm32")]
pub use rail::{Rail, RailItem};
#[cfg(target_arch = "wasm32")]
pub use search_input::SearchInput;
#[cfg(target_arch = "wasm32")]
pub use segmented::Segmented;
#[cfg(target_arch = "wasm32")]
pub use shell::Shell;
#[cfg(target_arch = "wasm32")]
pub use sparkline::Sparkline;
#[cfg(target_arch = "wasm32")]
pub use status_dot::StatusDot;
#[cfg(target_arch = "wasm32")]
pub use tabs::{TabItem, Tabs, TabsStyle, effective_active};
#[cfg(target_arch = "wasm32")]
pub use theme::{UiPrefs, install};
#[cfg(target_arch = "wasm32")]
pub use time::when::{When, WhenMode};
#[cfg(target_arch = "wasm32")]
pub use toast::{ToastBus, Toasts};
#[cfg(target_arch = "wasm32")]
pub use toggle::Toggle;
#[cfg(target_arch = "wasm32")]
pub use topbar::{AppLink, ModeTab, TopBar, UserInfo};
