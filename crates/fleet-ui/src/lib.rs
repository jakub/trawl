// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Leptos 0.8 design system for fleet apps (ADR-0030, step 2).
//!
//! Ships design tokens, reset + base styles, the theme preference
//! system, and four typed components (`Btn`, `Field`, `ConfirmModal`,
//! `Toasts`). Consumed by trawl-web-ui and the future coastwatch-web
//! via workspace path deps; CSS is consumed via Trunk's `data-trunk
//! rel="css"` directive pointing at `styles/fleet-ui.css` in this
//! crate's directory.
//!
//! Most modules are wasm32-only — they pull leptos / web-sys / gloo
//! and only make sense in a browser. The exceptions are [`theme::prefs`],
//! [`toast::kinds`], [`toast::stack`], [`button::variant`],
//! [`login::validate`], and the [`icon::Icon`] enum: pure layers that
//! build on every target so the `localStorage` JSON contract, the toast
//! CSS-class contract, the toast push/dismiss state machine, the button
//! CSS-class contract, the login key-validation + error-precedence
//! contracts, and the icon descriptor can be exercised by native unit
//! tests (and named by native consumer code). The `IconView` renderer
//! itself stays wasm32-only.

pub mod button;
pub mod login;
pub mod theme;
pub mod toast;

#[cfg(target_arch = "wasm32")]
pub mod drawer;
#[cfg(target_arch = "wasm32")]
pub mod error_banner;
#[cfg(target_arch = "wasm32")]
pub mod field;
pub mod icon;
#[cfg(target_arch = "wasm32")]
pub mod modal;
#[cfg(target_arch = "wasm32")]
pub mod rail;
#[cfg(target_arch = "wasm32")]
pub mod shell;
#[cfg(target_arch = "wasm32")]
pub mod tabs;
#[cfg(target_arch = "wasm32")]
pub mod topbar;

pub use button::{Size, Variant};
pub use icon::Icon;
pub use theme::{Density, RowStyle, Theme};
pub use toast::{Toast, ToastKind, ToastStack};

#[cfg(target_arch = "wasm32")]
pub use button::Btn;
#[cfg(target_arch = "wasm32")]
pub use drawer::Drawer;
#[cfg(target_arch = "wasm32")]
pub use error_banner::ErrorBanner;
#[cfg(target_arch = "wasm32")]
pub use field::{Field, Helper};
#[cfg(target_arch = "wasm32")]
pub use icon::IconView;
#[cfg(target_arch = "wasm32")]
pub use login::Login;
#[cfg(target_arch = "wasm32")]
pub use modal::{ConfirmModal, ConfirmWithReasonModal, Modal};
#[cfg(target_arch = "wasm32")]
pub use rail::{Rail, RailItem};
#[cfg(target_arch = "wasm32")]
pub use shell::Shell;
#[cfg(target_arch = "wasm32")]
pub use tabs::{TabItem, Tabs, TabsStyle};
#[cfg(target_arch = "wasm32")]
pub use theme::{UiPrefs, install};
#[cfg(target_arch = "wasm32")]
pub use toast::{ToastBus, Toasts};
#[cfg(target_arch = "wasm32")]
pub use topbar::{AppLink, ModeTab, TopBar, UserInfo};
