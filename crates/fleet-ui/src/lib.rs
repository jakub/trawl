// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared Leptos 0.8 design system for fleet apps (ADR-0030).
//!
//! Consumed by trawl-web-ui and coastwatch via path deps; CSS is
//! consumed via Trunk's `data-trunk rel="css"` directive pointing at
//! `styles/fleet-ui.css` in this crate's directory.

pub mod atmosphere;
pub mod badge;
pub mod button;
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
pub mod kbd;
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod menu;
#[cfg(target_arch = "wasm32")]
pub mod overflow_hint;
#[cfg(target_arch = "wasm32")]
pub mod pager;
#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod roving;
#[cfg(target_arch = "wasm32")]
pub mod search_input;
#[cfg(target_arch = "wasm32")]
pub mod shell;
pub mod sidebar;
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
pub use sidebar::{RailItem, SidebarGroup};
pub use sparkline::SparkPath;
pub use status_dot::StatusTone;
pub use theme::{Details, RowStyle, Rows, Theme, ThemePreference};
pub use toast::{Toast, ToastKind, ToastLink, ToastStack};

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
pub use overflow_hint::OverflowHint;
#[cfg(target_arch = "wasm32")]
pub use pager::Pager;
#[cfg(target_arch = "wasm32")]
pub use search_input::SearchInput;
#[cfg(target_arch = "wasm32")]
pub use segmented::Segmented;
#[cfg(target_arch = "wasm32")]
pub use shell::Shell;
#[cfg(target_arch = "wasm32")]
pub use sidebar::Sidebar;
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
pub use topbar::{TopBar, UserInfo};
