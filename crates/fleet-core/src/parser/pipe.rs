//! Layer 5: pipe stage parsers.
//!
//! Each pipe stage (`stats`, `where`, `sort`, `limit`, `table`) has its own
//! parser function. The pipeline parser chains them with `|` separators.
