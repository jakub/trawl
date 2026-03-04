//! Build-time version metadata.

use std::sync::LazyLock;

pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_SHA: &str = env!("TRAWL_GIT_SHA");
pub const GIT_DATE: &str = env!("TRAWL_GIT_DATE");
pub const GIT_DIRTY: &str = env!("TRAWL_GIT_DIRTY");
pub const RUSTC_VERSION: &str = env!("TRAWL_RUSTC_VERSION");
pub const TARGET_TRIPLE: &str = env!("TRAWL_TARGET_TRIPLE");

static LONG_VERSION: LazyLock<String> = LazyLock::new(|| {
    let dirty = if GIT_DIRTY == "true" { "*" } else { "" };
    format!("{PKG_VERSION} ({GIT_SHA}{dirty} {GIT_DATE}, rustc {RUSTC_VERSION}, {TARGET_TRIPLE})")
});

/// Returns the long version string for clap's `long_version` attribute.
pub fn long_version() -> &'static str {
    &LONG_VERSION
}
