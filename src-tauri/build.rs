fn main() {
    // Keep default tauri-build Windows app manifest for the shipped exe only.
    // Do not call WindowsAttributes::app_manifest / new_without_app_manifest here —
    // that would replace or risk duplicating the app RT_MANIFEST.
    tauri_build::build();
    embed_windows_common_controls_for_tests();
}

/// Windows cargo test harnesses are not app bins so they miss tauri-build's
/// Common-Controls v6 manifest. Without it the loader binds comctl32 5.82 and
/// dies on TaskDialogIndirect (STATUS_ENTRYPOINT_NOT_FOUND) before any test runs.
///
/// - `rustc-link-arg` reaches `cargo test --lib` harnesses (and the app link
///   line; duplicate SxS dependency merges harmlessly with Tauri's app manifest)
/// - `rustc-link-arg-tests` covers `tests/*.rs` integration binaries
///
/// See tauri-apps/tauri#13419.
fn embed_windows_common_controls_for_tests() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    let dep = "/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'";
    println!("cargo:rustc-link-arg={dep}");
    println!("cargo:rustc-link-arg-tests={dep}");
}
