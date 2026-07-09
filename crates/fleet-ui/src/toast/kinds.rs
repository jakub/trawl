// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure toast types. No `leptos`, no `web_sys`, no I/O — builds on
//! every target so consumers can name toast kinds from
//! native-compiling modules (and so `cargo nextest run --workspace`
//! exercises the CSS-class contract).

/// Semantic kind of a toast notification. Border-left color encodes
/// the kind — see `.toast.{info,success,error}` in
/// `styles/fleet-ui.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    /// CSS class suffix rendered by the `<Toasts/>` host as
    /// `class="toast {suffix}"`. The contract with fleet-ui.css's
    /// `.toast.info` / `.toast.success` / `.toast.error` selectors.
    #[must_use]
    pub fn as_class(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Error => "error",
        }
    }
}
