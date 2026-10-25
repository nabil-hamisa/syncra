//! Embed the application manifest (`app.manifest`): Common Controls v6
//! (modern buttons and edit boxes in the windows) and per-monitor-v2 DPI
//! awareness from process start, before any window or DXGI call exists.
fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{dir}\\app.manifest");
    }
}
