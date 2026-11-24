// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a query's input carries, known before any SQL is written
//! (ADR-0062).
//!
//! Every name here is folded through [`catalog_key`], `DuckDB`'s ASCII
//! identifier fold, so `Status` in a query and `status` in a file are one
//! column, as they are to `DuckDB`.

use std::collections::BTreeSet;

use crate::schema::catalog_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputColumns {
    pub cold: ColdColumns,
    pub hot: Option<HotColumns>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdColumns {
    /// Nobody asked, so every name binds.
    Unprobed,
    NoFiles,
    /// Each name folded through [`catalog_key`], with its `DuckDB` type text.
    /// Build it with [`ColdColumns::described`], which folds.
    Described(Vec<(String, String)>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotColumns {
    keys: BTreeSet<String>,
    null_only: BTreeSet<String>,
}

impl InputColumns {
    #[must_use]
    pub fn unprobed() -> Self {
        Self {
            cold: ColdColumns::Unprobed,
            hot: None,
        }
    }

    #[must_use]
    pub fn is_unprobed(&self) -> bool {
        matches!(self.cold, ColdColumns::Unprobed)
    }

    #[must_use]
    pub fn hot_only(&self) -> Self {
        let cold = match self.cold {
            ColdColumns::Unprobed => ColdColumns::Unprobed,
            ColdColumns::NoFiles | ColdColumns::Described(_) => ColdColumns::NoFiles,
        };
        Self {
            cold,
            hot: self.hot.clone(),
        }
    }

    #[must_use]
    pub fn carries(&self, name: &str) -> bool {
        self.cold.carries(name) || self.hot.as_ref().is_some_and(|hot| hot.contains(name))
    }

    #[must_use]
    pub fn cold_type(&self, name: &str) -> Option<&str> {
        self.cold.type_of(name)
    }
}

impl ColdColumns {
    pub fn described<N, T>(columns: impl IntoIterator<Item = (N, T)>) -> Self
    where
        N: AsRef<str>,
        T: Into<String>,
    {
        Self::Described(
            columns
                .into_iter()
                .map(|(name, ty)| (catalog_key(name.as_ref()), ty.into()))
                .collect(),
        )
    }

    #[must_use]
    pub fn carries(&self, name: &str) -> bool {
        match self {
            Self::Unprobed => true,
            Self::NoFiles => false,
            Self::Described(_) => self.type_of(name).is_some(),
        }
    }

    #[must_use]
    pub fn type_of(&self, name: &str) -> Option<&str> {
        match self {
            Self::Unprobed | Self::NoFiles => None,
            Self::Described(columns) => columns
                .iter()
                .find(|(column, _)| column.eq_ignore_ascii_case(name))
                .map(|(_, ty)| ty.as_str()),
        }
    }
}

impl HotColumns {
    pub fn new(
        keys: impl IntoIterator<Item = impl AsRef<str>>,
        null_only: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        let null_only: BTreeSet<String> = null_only
            .into_iter()
            .map(|key| catalog_key(key.as_ref()))
            .collect();
        let mut keys: BTreeSet<String> = keys
            .into_iter()
            .map(|key| catalog_key(key.as_ref()))
            .collect();
        keys.extend(null_only.iter().cloned());
        Self { keys, null_only }
    }

    pub fn keys_only(keys: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self::new(keys, std::iter::empty::<&str>())
    }

    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.keys.contains(&catalog_key(name))
    }

    #[must_use]
    pub fn is_null_only(&self, name: &str) -> bool {
        self.null_only.contains(&catalog_key(name))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(String::as_str)
    }

    pub fn null_only(&self) -> impl Iterator<Item = &str> {
        self.null_only.iter().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cold() -> ColdColumns {
        ColdColumns::described([
            ("_time", "TIMESTAMP"),
            ("Host", "VARCHAR"),
            ("status", "BIGINT"),
        ])
    }

    #[test]
    fn described_names_fold_and_keep_their_order_and_types() {
        assert_eq!(
            cold(),
            ColdColumns::Described(vec![
                ("_time".to_owned(), "TIMESTAMP".to_owned()),
                ("host".to_owned(), "VARCHAR".to_owned()),
                ("status".to_owned(), "BIGINT".to_owned()),
            ])
        );
    }

    #[test]
    fn a_cold_lookup_folds_the_name_it_is_asked() {
        let cold = cold();
        for name in ["host", "HOST", "Host"] {
            assert!(cold.carries(name), "{name}");
            assert_eq!(cold.type_of(name), Some("VARCHAR"), "{name}");
        }
        assert_eq!(cold.type_of("STATUS"), Some("BIGINT"));
        assert!(!cold.carries("f"));
        assert_eq!(cold.type_of("f"), None);
    }

    #[test]
    fn a_hand_built_description_is_looked_up_without_regard_to_case() {
        let cold = ColdColumns::Described(vec![("Host".to_owned(), "VARCHAR".to_owned())]);
        assert_eq!(cold.type_of("host"), Some("VARCHAR"));
    }

    #[test]
    fn unprobed_binds_every_name_and_no_files_binds_none() {
        assert!(ColdColumns::Unprobed.carries("anything"));
        assert_eq!(ColdColumns::Unprobed.type_of("anything"), None);
        assert!(!ColdColumns::NoFiles.carries("_time"));
        assert_eq!(ColdColumns::NoFiles.type_of("_time"), None);

        let unprobed = InputColumns::unprobed();
        assert!(unprobed.is_unprobed());
        assert_eq!(unprobed.hot, None);
        assert!(unprobed.carries("nosuch"));
    }

    #[test]
    fn hot_keys_fold_and_null_only_keys_are_keys() {
        let hot = HotColumns::new(["Host", "f", "_time"], ["F", "g"]);
        assert_eq!(hot.keys().collect::<Vec<_>>(), ["_time", "f", "g", "host"]);
        assert_eq!(hot.null_only().collect::<Vec<_>>(), ["f", "g"]);
        assert!(hot.contains("HOST"));
        assert!(hot.contains("G"));
        assert!(!hot.contains("status"));
        assert!(hot.is_null_only("f"));
        assert!(!hot.is_null_only("host"));
        assert!(!hot.is_null_only("status"));

        let keys_only = HotColumns::keys_only(["Host", "f"]);
        assert_eq!(keys_only.keys().collect::<Vec<_>>(), ["f", "host"]);
        assert_eq!(keys_only.null_only().count(), 0);
        assert_eq!(HotColumns::default().keys().count(), 0);
    }

    #[test]
    fn a_probed_input_carries_the_union_of_cold_and_hot() {
        let input = InputColumns {
            cold: cold(),
            hot: Some(HotColumns::keys_only(["host", "user"])),
        };
        assert!(!input.is_unprobed());
        assert!(input.carries("status"), "cold only");
        assert!(input.carries("USER"), "hot only");
        assert!(input.carries("host"), "both");
        assert!(!input.carries("f"));
        assert_eq!(input.cold_type("Status"), Some("BIGINT"));
        assert_eq!(input.cold_type("user"), None, "hot keys have no cold type");

        let cold_less = InputColumns {
            cold: ColdColumns::NoFiles,
            hot: None,
        };
        assert!(!cold_less.carries("host"));
    }

    #[test]
    fn hot_only_drops_the_cold_side_of_a_probed_input() {
        let hot = HotColumns::keys_only(["host"]);
        let input = InputColumns {
            cold: cold(),
            hot: Some(hot.clone()),
        };
        let narrowed = input.hot_only();
        assert_eq!(narrowed.cold, ColdColumns::NoFiles);
        assert_eq!(narrowed.hot, Some(hot.clone()));
        assert!(!narrowed.carries("status"));
        assert!(narrowed.carries("host"));

        let unprobed = InputColumns {
            cold: ColdColumns::Unprobed,
            hot: Some(hot.clone()),
        };
        assert_eq!(unprobed.hot_only(), unprobed);
        assert!(InputColumns::unprobed().hot_only().is_unprobed());
    }
}
