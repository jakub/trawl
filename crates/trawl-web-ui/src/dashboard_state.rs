// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Dashboard freshness and bootstrap precedence, independent of browser callbacks.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashboardPhase {
    Waiting,
    Bootstrap,
    Live,
    Stale,
    Forbidden,
    Failed,
}

impl DashboardPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting for first snapshot",
            Self::Bootstrap => "Snapshot; waiting for live updates",
            Self::Live => "Live",
            Self::Stale => "Stale; reconnecting",
            Self::Forbidden => "Forbidden",
            Self::Failed => "Live updates failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DashboardState<T> {
    pub phase: DashboardPhase,
    pub snapshot: Option<T>,
    stream_seen: bool,
}

impl<T> Default for DashboardState<T> {
    fn default() -> Self {
        Self {
            phase: DashboardPhase::Waiting,
            snapshot: None,
            stream_seen: false,
        }
    }
}

impl<T> DashboardState<T> {
    pub fn stream_snapshot(&mut self, snapshot: T) {
        self.stream_seen = true;
        self.snapshot = Some(snapshot);
        self.phase = DashboardPhase::Live;
    }

    pub fn bootstrap(&mut self, result: Result<T, Option<u16>>) {
        if self.stream_seen {
            return;
        }
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                if self.phase == DashboardPhase::Waiting {
                    self.phase = DashboardPhase::Bootstrap;
                }
            }
            Err(Some(503)) => {}
            Err(Some(401 | 403)) => {
                self.snapshot = None;
                self.phase = DashboardPhase::Forbidden;
            }
            Err(_) => {
                self.phase = DashboardPhase::Failed;
            }
        }
    }

    pub fn stream_error(&mut self, reconnecting: bool) {
        self.phase = if reconnecting {
            DashboardPhase::Stale
        } else {
            DashboardPhase::Failed
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_authority_survives_delayed_bootstrap_and_reconnect() {
        for result in [Ok(1), Err(Some(503)), Err(Some(403)), Err(None)] {
            let mut state = DashboardState::default();
            state.stream_snapshot(2);
            state.bootstrap(result);
            assert_eq!(state.snapshot, Some(2));
            assert_eq!(state.phase, DashboardPhase::Live);
            state.stream_error(true);
            state.bootstrap(Ok(1));
            assert_eq!(state.phase, DashboardPhase::Stale);
            assert_eq!(state.snapshot, Some(2));
            state.stream_snapshot(3);
            assert_eq!(state.phase, DashboardPhase::Live);
        }
    }
    #[test]
    fn waiting_503_recovers_and_bootstrap_never_claims_live() {
        let mut state = DashboardState::default();
        state.bootstrap(Err(Some(503)));
        assert_eq!(state.phase, DashboardPhase::Waiting);
        state.bootstrap(Ok(1));
        assert_eq!(state.phase, DashboardPhase::Bootstrap);
        state.stream_error(true);
        state.bootstrap(Ok(2));
        assert_eq!(state.phase, DashboardPhase::Stale);
        state.stream_snapshot(3);
        assert_eq!(state.phase, DashboardPhase::Live);
    }
}
