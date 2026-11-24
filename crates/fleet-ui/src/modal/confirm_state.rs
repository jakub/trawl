// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure confirm-dialog state.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmState<T>(Option<T>);

impl<T> Default for ConfirmState<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> ConfirmState<T> {
    pub fn request(&mut self, payload: T) {
        self.0 = Some(payload);
    }

    pub fn cancel(&mut self) {
        self.0 = None;
    }

    pub fn take(&mut self) -> Option<T> {
        self.0.take()
    }

    #[must_use]
    pub fn pending(&self) -> Option<&T> {
        self.0.as_ref()
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::ConfirmState;

    #[test]
    fn starts_closed() {
        let state: ConfirmState<(i64, String)> = ConfirmState::default();
        assert!(!state.is_open());
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn request_opens_and_exposes_the_payload() {
        let mut state = ConfirmState::default();
        state.request((7_i64, "alpha".to_string()));
        assert!(state.is_open());
        assert_eq!(state.pending(), Some(&(7, "alpha".to_string())));

        state.request((9, "beta".to_string()));
        assert_eq!(state.pending(), Some(&(9, "beta".to_string())));
    }

    #[test]
    fn cancel_closes_and_drops() {
        let mut state = ConfirmState::default();
        state.request(42_u32);
        state.cancel();
        assert!(!state.is_open());
        assert_eq!(state.take(), None, "cancelled payload must not leak");
    }

    #[test]
    fn take_confirms_exactly_once() {
        let mut state = ConfirmState::default();
        state.request(42_u32);
        assert_eq!(state.take(), Some(42));
        assert!(!state.is_open(), "take closes the dialog");
        assert_eq!(state.take(), None, "second take is a no-op");
    }
}
