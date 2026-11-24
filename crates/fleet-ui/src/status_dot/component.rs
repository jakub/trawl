// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::tone::{StatusTone, dot_class};

#[component]
pub fn StatusDot(#[prop(optional)] tone: StatusTone) -> impl IntoView {
    view! {
        <span class=dot_class(tone) aria-hidden="true"></span>
    }
}
