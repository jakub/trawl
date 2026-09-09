// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Offset pagination over one returned server page. Fetching, page size,
//! filtering and URL state belong to the caller.

use std::num::NonZeroUsize;

/// Whether the server reports the whole result count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageTotal {
    Known(usize),
    /// A full page offers Next, including a full final page. The resulting
    /// empty page keeps Prev and its page index; it never bounces back.
    Probe,
}

/// The requested offset or returned row range cannot fit in `usize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindowOverflow;

/// The page that produced the rows, independent of a pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageWindow {
    page: usize,
    size: NonZeroUsize,
    returned: usize,
    total: PageTotal,
    first: usize,
    last: usize,
    busy: bool,
}

impl PageWindow {
    /// Compute an offset before sending a request.
    ///
    /// # Errors
    /// Returns an error when multiplication would overflow.
    pub fn checked_offset(page: usize, size: NonZeroUsize) -> Result<usize, PageWindowOverflow> {
        page.checked_mul(size.get()).ok_or(PageWindowOverflow)
    }

    /// Describe the returned page. Known totals clamp the row range, never
    /// the fetched page: an empty out-of-range response stays empty, and
    /// Prev targets the last valid page without inventing its rows.
    ///
    /// # Errors
    /// Returns an error when the offset or returned range cannot fit.
    pub fn new(
        page: usize,
        size: NonZeroUsize,
        returned: usize,
        total: PageTotal,
        busy: bool,
    ) -> Result<Self, PageWindowOverflow> {
        let offset = Self::checked_offset(page, size)?;
        let returned = match total {
            PageTotal::Known(total) => returned.min(size.get()).min(total.saturating_sub(offset)),
            PageTotal::Probe => returned.min(size.get()),
        };
        let (first, last) = if returned == 0 {
            (0, 0)
        } else {
            (
                offset.checked_add(1).ok_or(PageWindowOverflow)?,
                offset.checked_add(returned).ok_or(PageWindowOverflow)?,
            )
        };
        Ok(Self {
            page,
            size,
            returned,
            total,
            first,
            last,
            busy,
        })
    }

    #[must_use]
    pub const fn first(self) -> usize {
        self.first
    }

    #[must_use]
    pub const fn last(self) -> usize {
        self.last
    }

    #[must_use]
    pub fn prev_page(self) -> Option<usize> {
        if self.busy || self.total == PageTotal::Known(0) {
            return None;
        }
        let previous = self.page.checked_sub(1)?;
        Some(match self.total {
            PageTotal::Known(total) => previous.min(total.saturating_sub(1) / self.size.get()),
            PageTotal::Probe => previous,
        })
    }

    #[must_use]
    pub fn next_page(self) -> Option<usize> {
        if self.busy {
            return None;
        }
        let next = self.page.checked_add(1)?;
        let offset = Self::checked_offset(next, self.size).ok()?;
        // A nonempty next page must be able to name its first row.
        offset.checked_add(1)?;
        let has_next = match self.total {
            PageTotal::Known(total) => self.returned != 0 && offset < total,
            PageTotal::Probe => self.returned == self.size.get(),
        };
        has_next.then_some(next)
    }

    #[must_use]
    pub fn can_prev(self) -> bool {
        self.prev_page().is_some()
    }

    #[must_use]
    pub fn can_next(self) -> bool {
        self.next_page().is_some()
    }

    #[must_use]
    pub fn summary(self) -> String {
        match self.total {
            PageTotal::Known(0) => "0 entries".into(),
            PageTotal::Known(total) => format!("{}–{} of {total}", self.first, self.last),
            PageTotal::Probe => format!(
                "Page {} · showing {} {}",
                self.page as u128 + 1,
                self.returned,
                if self.returned == 1 { "row" } else { "rows" },
            ),
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod component {
    use super::PageWindow;
    use crate::Pager;
    use leptos::prelude::*;

    /// Render a page window using the shared table footer. Re-read the
    /// window on click so a newly pending request disables navigation
    /// even before the browser paints the disabled attribute.
    #[component]
    pub fn OffsetPager(
        #[prop(into)] window: Signal<PageWindow>,
        on_page: Callback<usize>,
        #[prop(into, optional)] suffix: Signal<String>,
    ) -> impl IntoView {
        view! {
            <Pager
                summary=Signal::derive(move || format!("{}{}", window.get().summary(), suffix.get()))
                can_prev=Signal::derive(move || window.get().can_prev())
                can_next=Signal::derive(move || window.get().can_next())
                on_prev=Callback::new(move |()| {
                    if let Some(page) = window.get_untracked().prev_page() { on_page.run(page); }
                })
                on_next=Callback::new(move |()| {
                    if let Some(page) = window.get_untracked().next_page() { on_page.run(page); }
                })
            />
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use component::OffsetPager;

#[cfg(test)]
mod tests {
    use super::*;

    fn window(page: usize, returned: usize, total: PageTotal) -> PageWindow {
        PageWindow::new(page, NonZeroUsize::new(50).unwrap(), returned, total, false).unwrap()
    }

    #[test]
    fn page_window_known_boundaries() {
        for (page, returned, total, first, last, prev, next) in [
            (0, 0, 0, 0, 0, None, None),
            (0, 1, 1, 1, 1, None, None),
            (0, 50, 100, 1, 50, None, Some(1)),
            (1, 50, 100, 51, 100, Some(0), None),
            (2, 3, 103, 101, 103, Some(1), None),
            (2, 50, 103, 101, 103, Some(1), None),
            (9, 0, 103, 0, 0, Some(2), None),
            (9, 0, 0, 0, 0, None, None),
            (1, 0, 103, 0, 0, Some(0), None),
        ] {
            let w = window(page, returned, PageTotal::Known(total));
            assert_eq!((w.first(), w.last()), (first, last));
            assert_eq!((w.prev_page(), w.next_page()), (prev, next));
            assert!(w.first() <= w.last());
        }
        assert_eq!(window(9, 0, PageTotal::Known(103)).summary(), "0–0 of 103");
        assert_eq!(window(0, 0, PageTotal::Known(0)).summary(), "0 entries");
    }

    #[test]
    fn page_window_probe_keeps_empty_page() {
        assert!(window(0, 50, PageTotal::Probe).can_next());
        assert!(!window(1, 3, PageTotal::Probe).can_next());
        let empty = window(1, 0, PageTotal::Probe);
        assert_eq!(empty.summary(), "Page 2 · showing 0 rows");
        assert_eq!(empty.prev_page(), Some(0));
        assert_eq!(empty.next_page(), None);
        assert_eq!(
            window(0, 1, PageTotal::Probe).summary(),
            "Page 1 · showing 1 row"
        );
    }

    #[test]
    fn page_window_busy_disables_both_directions() {
        let w = PageWindow::new(
            1,
            NonZeroUsize::new(50).unwrap(),
            50,
            PageTotal::Known(150),
            true,
        )
        .unwrap();
        assert!(!w.can_prev());
        assert!(!w.can_next());
        assert_eq!(w.summary(), "51–100 of 150");
    }

    #[test]
    fn page_window_checked_arithmetic_at_usize_limits() {
        let one = NonZeroUsize::new(1).unwrap();
        let two = NonZeroUsize::new(2).unwrap();
        assert!(NonZeroUsize::new(0).is_none());
        assert_eq!(PageWindow::checked_offset(usize::MAX, one), Ok(usize::MAX));
        assert_eq!(
            PageWindow::checked_offset(usize::MAX, two),
            Err(PageWindowOverflow)
        );
        assert!(PageWindow::new(usize::MAX, one, 1, PageTotal::Probe, false).is_err());
        let fifty = NonZeroUsize::new(50).unwrap();
        assert!(PageWindow::new(usize::MAX / 50, fifty, 50, PageTotal::Probe, false).is_err());
        let empty = PageWindow::new(usize::MAX, one, 0, PageTotal::Probe, false).unwrap();
        assert!(empty.can_prev());
        assert!(!empty.can_next());
        let last =
            PageWindow::new(usize::MAX - 1, one, 1, PageTotal::Known(usize::MAX), false).unwrap();
        assert_eq!(last.last(), usize::MAX);
        assert!(!last.can_next());
    }
}
