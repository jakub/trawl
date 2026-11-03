//! The one owner of regex construction (ADR-0063).
//!
//! `price` measures a pattern's translation and repeat cost from its
//! syntax tree before anything is built. `table` holds the patterns a
//! compiled live stage looks up.

pub(crate) mod price;
pub(crate) mod table;
