//! Layer 4: expression parser with operator precedence.
//!
//! Recursive descent with `foldl`/`foldr` for 7 precedence levels.
//! Used in `where` clauses, aggregation arguments, and `in` lists.
