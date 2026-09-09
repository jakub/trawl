// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Formula-injection protection for CSV text cells.

use std::borrow::Cow;

/// Prefix cell values that could trigger formula injection in spreadsheets.
///
/// See OWASP CSV injection guidelines. Only string values need
/// sanitization — numeric values like `-42` are legitimately negative.
/// Returns borrowed when no prefix is needed.
pub fn sanitize_csv_formula(s: &str) -> Cow<'_, str> {
    if s.starts_with(['=', '+', '-', '@', '\t', '|']) {
        Cow::Owned(format!("'{s}"))
    } else {
        Cow::Borrowed(s)
    }
}
