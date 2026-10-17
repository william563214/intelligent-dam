fn main() {
    tauri_build::build();
    embed_windows_common_controls_for_tests();
}

/// Windows cargo test harnesses are not app bins so they miss tauri-build's
/// Common-Controls v6 manifest. Without it the loader binds comctl32 5.82 and
/// dies on TaskDialogIndirect (STATUS_ENTRYPOINT_NOT_FOUND) before any test runs.
/// `rustc-link-arg` reaches lib unit-test binaries; `rustc-link-arg-tests` alone
/// does not. Duplicate dependency on the real app exe is merged harmlessly.
fn embed_windows_common_controls_for_tests() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows-msvc") {
        return;
    }
    println!(
        "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
