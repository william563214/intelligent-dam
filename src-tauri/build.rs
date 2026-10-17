fn main() {
    // Keep default tauri-build Windows app manifest for the shipped exe only.
    // Do not call WindowsAttributes::app_manifest / new_without_app_manifest here —
    // that would replace or risk duplicating the app RT_MANIFEST.
    tauri_build::build();
    embed_windows_common_controls_for_integration_tests();
}

/// Scope Common-Controls v6 to Cargo *integration* test binaries only
/// (`cargo:rustc-link-arg-tests`), leaving the release/app link line unchanged.
///
/// Root cause: `tauri-plugin-dialog` / `rfd` import `TaskDialogIndirect`, a
/// comctl32 **v6-only** export. Tauri embeds the v6 SxS dependency in the app
/// exe via winres; `tests/*.rs` harnesses are not bins so they bind System32
/// comctl32 5.82 and die at load with STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139).
/// See tauri-apps/tauri#13419.
///
/// Note: `rustc-link-arg-tests` does not apply to `cargo test --lib` harnesses.
/// Those already pass on Windows for this crate; only `tests/` needed the fix.
fn embed_windows_common_controls_for_integration_tests() {
    // Prefer Cargo cfg env vars over `cfg!(windows)` so cross-compile host
    // does not decide; only MSVC Windows targets need the SxS link arg.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    println!(
        "cargo:rustc-link-arg-tests=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
