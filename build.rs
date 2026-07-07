//! Windows resource embedding: icon + FileVersion/ProductVersion for BOTH
//! bins (pulse.exe and pulse-ctl.exe — winresource links the same
//! compiled resource into every binary target).
//!
//! No-ops gracefully if `assets/icon.ico` is ever absent, so `cargo build`
//! never needs the image toolchain (resvg/ImageMagick); the icon is
//! committed, so a normal clone always takes the resource path.
//! FileVersion/ProductVersion default from CARGO_PKG_VERSION — Cargo.toml
//! stays the single version source.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    if !std::path::Path::new("assets/icon.ico").exists() {
        // Defensive: no icon, no resource. Version resources without an
        // icon would still be nice, but winresource needs windres/rc
        // plumbing either way — keep a no-icon build a plain no-op.
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    // Task Manager / file-properties display names (the crate id is the
    // lowercase "pulse"; the product is "Pulse").
    res.set("ProductName", "Pulse");
    res.set("FileDescription", "Pulse");
    res.compile().expect("windows resource (icon/version) failed to compile");
}
