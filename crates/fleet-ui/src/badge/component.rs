// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::tone::{Tone, badge_class};

#[component]
pub fn Badge(#[prop(optional)] tone: Tone, children: Children) -> impl IntoView {
    view! {
        <span class=badge_class(tone)>{children()}</span>
    }
}
