// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

pub const RESOLVER_SCHEMA: u32 = 1;
pub const MAX_RESOLVER_OUTPUT_BYTES: usize = 1024 * 1024;

/// Secret value that is redacted in diagnostics and zeroized on drop.
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolverDocument {
    schema: u32,
    values: UniqueStringMap,
}

#[derive(Debug, Default)]
struct UniqueStringMap(BTreeMap<String, String>);

impl<'de> Deserialize<'de> for UniqueStringMap {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueMapVisitor;

        impl<'de> Visitor<'de> for UniqueMapVisitor {
            type Value = UniqueStringMap;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique string keys and string values")
            }

            fn visit_map<A>(self, mut access: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((name, value)) = access.next_entry::<String, String>()? {
                    if values.insert(name.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate resolver field {name:?}"
                        )));
                    }
                }
                Ok(UniqueStringMap(values))
            }
        }

        deserializer.deserialize_map(UniqueMapVisitor)
    }
}

pub fn parse_output(app: &str, bytes: &[u8]) -> Result<BTreeMap<String, SecretValue>> {
    if bytes.len() > MAX_RESOLVER_OUTPUT_BYTES {
        return Err(Error::ResolverProtocol {
            app: app.to_owned(),
            message: format!("stdout exceeds the {MAX_RESOLVER_OUTPUT_BYTES} byte protocol limit"),
        });
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut document = ResolverDocument::deserialize(&mut deserializer).map_err(|source| {
        Error::ResolverProtocol {
            app: app.to_owned(),
            message: source.to_string(),
        }
    })?;
    deserializer
        .end()
        .map_err(|source| Error::ResolverProtocol {
            app: app.to_owned(),
            message: format!("extra stdout after JSON document: {source}"),
        })?;
    if document.schema != RESOLVER_SCHEMA {
        return Err(Error::ResolverProtocol {
            app: app.to_owned(),
            message: format!(
                "unsupported schema {}; expected {RESOLVER_SCHEMA}",
                document.schema
            ),
        });
    }
    // Wrap every value before the first fallible step, so a rejected field
    // name still leaves the rest zeroized on the way out.
    let parsed: BTreeMap<String, SecretValue> = std::mem::take(&mut document.values.0)
        .into_iter()
        .map(|(name, value)| (name, SecretValue::new(value)))
        .collect();
    let mut output = BTreeMap::new();
    for (name, value) in parsed {
        validate_name(app, &name)?;
        // Source names are unique and `app.` prefixing is injective, so the
        // destination cannot collide.
        output.insert(format!("app.{name}"), value);
    }
    Ok(output)
}

fn validate_name(app: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with("app.")
        || name.starts_with("fleet.")
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(Error::ResolverProtocol {
            app: app.to_owned(),
            message: format!(
                "field {name:?} must be a lowercase logical name without a reserved namespace"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_document_is_namespaced() {
        let values = parse_output(
            "coastwatch",
            br#"{"schema":1,"values":{"openrouter_api_key":"secret"}}"#,
        )
        .unwrap();
        assert_eq!(values["app.openrouter_api_key"].expose(), "secret");
        assert_eq!(
            format!("{:?}", values["app.openrouter_api_key"]),
            "<redacted>"
        );
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let error =
            parse_output("coastwatch", br#"{"schema":1,"values":{},"surprise":true}"#).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_non_string_values() {
        let error = parse_output("coastwatch", br#"{"schema":1,"values":{"key":42}}"#).unwrap_err();
        assert!(error.to_string().contains("invalid type"));
    }

    #[test]
    fn rejects_extra_stdout() {
        let error = parse_output("coastwatch", b"{\"schema\":1,\"values\":{}}\nnoise").unwrap_err();
        assert!(error.to_string().contains("extra stdout"));
    }

    #[test]
    fn rejects_schema_mismatch_and_reserved_names() {
        assert!(parse_output("x", br#"{"schema":2,"values":{}}"#).is_err());
        assert!(
            parse_output(
                "x",
                br#"{"schema":1,"values":{"fleet.session_aead_key":"bad"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_fields_and_oversized_output() {
        let duplicate = br#"{"schema":1,"values":{"key":"first","key":"second"}}"#;
        let error = parse_output("x", duplicate).unwrap_err();
        assert!(error.to_string().contains("duplicate resolver field"));

        let oversized = vec![b' '; MAX_RESOLVER_OUTPUT_BYTES + 1];
        let error = parse_output("x", &oversized).unwrap_err();
        assert!(error.to_string().contains("protocol limit"));
    }
}
