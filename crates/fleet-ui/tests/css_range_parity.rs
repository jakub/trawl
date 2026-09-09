// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod common;

#[test]
fn moved_range_rules_keep_their_exact_declarations() {
    let golden = common::rules(include_str!("fixtures/premigration-range.css"));
    assert_eq!(golden.len(), 27, "the whole captured range rule set");
    let css = include_str!("../styles/fleet-ui.css");
    for rule in golden {
        assert!(css.contains(&rule), "range rule changed: {rule}");
    }
}
