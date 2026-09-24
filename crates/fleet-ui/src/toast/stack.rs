// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure toast-stack state machine. No `leptos`, no `web_sys`, no I/O —
//! builds on every target so the "a success toast and an error toast
//! are now live" outcome is enforced by `cargo nextest run
//! --workspace`, not just eyeballed in a browser.
//!
//! [`runtime`](super::runtime) wraps a [`ToastStack`] in a reactive
//! `RwSignal` and drives auto-dismiss; every mutation it performs is a
//! call into this module. Keeping the id allocation + push/dismiss
//! transitions here means they're unit-tested directly.

use super::kinds::ToastKind;

/// A destination a toast offers, such as the record an operation just
/// created. The host renders it as a native anchor after the detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastLink {
    href: String,
    label: String,
}

impl ToastLink {
    #[must_use]
    pub fn new(href: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            href: href.into(),
            label: label.into(),
        }
    }

    #[must_use]
    pub fn href(&self) -> &str {
        &self.href
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// A single toast notification. Construction is sealed: instances only
/// arise from [`ToastStack::push`], so the monotonic `id` allocated by
/// the stack is the only one in circulation — preventing a third-party
/// `Toast { id: 0, ... }` from colliding with the keys the `<For>` loop
/// in [`runtime`](super::runtime) relies on.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Toast {
    pub(crate) id: u64,
    pub(crate) kind: ToastKind,
    pub(crate) title: String,
    pub(crate) detail: Option<String>,
    pub(crate) link: Option<ToastLink>,
}

impl Toast {
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn kind(&self) -> ToastKind {
        self.kind
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    #[must_use]
    pub fn link(&self) -> Option<&ToastLink> {
        self.link.as_ref()
    }
}

/// Ordered stack of live toasts plus the monotonic id counter. Pure
/// value type — `runtime`'s `ToastBus` holds one inside an `RwSignal`
/// and all its mutations funnel through [`Self::push`] / [`Self::dismiss`].
#[derive(Debug, Clone, Default)]
pub struct ToastStack {
    items: Vec<Toast>,
    next_id: u64,
}

impl ToastStack {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next monotonic id, append a toast of `kind`, and
    /// return the id so the caller can schedule a later [`Self::dismiss`].
    /// Ids are never reused, so they stay unique as `<For>` keys even
    /// after intervening dismissals.
    pub fn push(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        detail: Option<String>,
    ) -> u64 {
        self.append(kind, title.into(), detail, None)
    }

    /// [`Self::push`] for a toast that also offers `link`.
    pub fn push_with_link(
        &mut self,
        kind: ToastKind,
        title: impl Into<String>,
        detail: Option<String>,
        link: ToastLink,
    ) -> u64 {
        self.append(kind, title.into(), detail, Some(link))
    }

    fn append(
        &mut self,
        kind: ToastKind,
        title: String,
        detail: Option<String>,
        link: Option<ToastLink>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(Toast {
            id,
            kind,
            title,
            detail,
            link,
        });
        id
    }

    /// Remove the toast with `id`. No-op if it's already gone (e.g. the
    /// user dismissed it before the auto-dismiss timeout fired).
    pub fn dismiss(&mut self, id: u64) {
        self.items.retain(|t| t.id != id);
    }

    /// Live toasts, oldest first — the render order of the `<Toasts/>`
    /// host.
    #[must_use]
    pub fn items(&self) -> &[Toast] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No browser required: pushing a success then an error leaves
    /// exactly those two toasts live, in order, with the kinds the app
    /// asked for.
    #[test]
    fn success_then_error_are_both_live() {
        let mut stack = ToastStack::new();
        stack.push(ToastKind::Success, "Saved", Some("net created".into()));
        stack.push(ToastKind::Error, "Export failed", Some("disk full".into()));

        let kinds: Vec<ToastKind> = stack.items().iter().map(Toast::kind).collect();
        assert_eq!(kinds, vec![ToastKind::Success, ToastKind::Error]);
        assert_eq!(stack.items()[0].title(), "Saved");
        assert_eq!(stack.items()[1].detail(), Some("disk full"));
    }

    #[test]
    fn ids_are_monotonic_and_unique() {
        let mut stack = ToastStack::new();
        let a = stack.push(ToastKind::Info, "a", None);
        let b = stack.push(ToastKind::Info, "b", None);
        assert_eq!((a, b), (1, 2));
    }

    #[test]
    fn dismiss_removes_only_the_named_toast_and_ids_never_reuse() {
        let mut stack = ToastStack::new();
        let a = stack.push(ToastKind::Success, "a", None);
        let b = stack.push(ToastKind::Error, "b", None);
        stack.dismiss(a);

        assert_eq!(stack.items().len(), 1);
        assert_eq!(stack.items()[0].id(), b);

        // A later push does not recycle the dismissed id.
        let c = stack.push(ToastKind::Info, "c", None);
        assert_eq!(c, 3);
    }

    /// A linked toast carries its destination and label; a plain push
    /// carries none, so existing callers render exactly as before.
    #[test]
    fn a_linked_toast_carries_its_link_and_a_plain_one_none() {
        let mut stack = ToastStack::new();
        stack.push(ToastKind::Info, "plain", None);
        let id = stack.push_with_link(
            ToastKind::Success,
            "Run started",
            Some("covers 14:05–14:21 UTC".into()),
            ToastLink::new("/jobs/runs?run=7&net=3", "View run"),
        );

        assert_eq!(stack.items()[0].link(), None);
        let linked = &stack.items()[1];
        assert_eq!(linked.id(), id);
        assert_eq!(linked.kind(), ToastKind::Success);
        assert_eq!(linked.detail(), Some("covers 14:05–14:21 UTC"));
        let link = linked.link().expect("the linked toast keeps its link");
        assert_eq!(link.href(), "/jobs/runs?run=7&net=3");
        assert_eq!(link.label(), "View run");
    }

    /// Linked and plain toasts share one id sequence, so `<For>` keys
    /// stay unique across both kinds of push.
    #[test]
    fn linked_and_plain_pushes_share_one_id_sequence() {
        let mut stack = ToastStack::new();
        let a = stack.push(ToastKind::Info, "a", None);
        let b = stack.push_with_link(ToastKind::Info, "b", None, ToastLink::new("/x", "x"));
        let c = stack.push(ToastKind::Info, "c", None);
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn dismiss_unknown_id_is_a_noop() {
        let mut stack = ToastStack::new();
        stack.push(ToastKind::Info, "a", None);
        stack.dismiss(999);
        assert_eq!(stack.items().len(), 1);
    }
}
