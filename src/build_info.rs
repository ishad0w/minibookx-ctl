//! Compile-time provenance. This module never reads the runtime environment.
pub const COMMIT: &str = env!("MINIBOOKX_BUILD_COMMIT");
pub const TARGET: &str = env!("MINIBOOKX_BUILD_TARGET");
pub const LIBDBUS_STATIC_SHA256: &str = env!("MINIBOOKX_LIBDBUS_SHA256");
pub const RUSTC: &str = env!("MINIBOOKX_BUILD_RUSTC");
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+g",
    env!("MINIBOOKX_BUILD_SHORT"),
    env!("MINIBOOKX_BUILD_DIRTY_SUFFIX")
);
pub const DIRTY: bool = !env!("MINIBOOKX_BUILD_DIRTY_SUFFIX").is_empty();

pub fn json() -> String {
    // Provenance is captured by Cargo's build script, never the runtime env.
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"), "binary_version": VERSION,
        "commit": COMMIT, "target": TARGET, "dirty": DIRTY, "rustc": RUSTC,
        "libdbus_static_sha256": (!LIBDBUS_STATIC_SHA256.is_empty()).then_some(LIBDBUS_STATIC_SHA256),
    })
    .to_string()
}
