//! pulse-ctl — the Pulse controller CLI (console subsystem).
//!
//! A SEPARATE binary on purpose: the main exe is `windows_subsystem =
//! "windows"` in release, so PowerShell does not wait for it and its stdout
//! is LOST (documented ops incident — probes need Start-Process -Wait). A
//! controller CLI that loses its output is dead on arrival, so this one is a
//! real console program with no subsystem attribute.
//!
//! The dependency closure is exactly protocol → state (+ strip and the CLI
//! itself): no daemon, no GUI, no egui — `crate::state` inside protocol.rs
//! resolves to this bin crate's own `mod state`.

// The shared modules carry plenty of API this thin binary never calls
// (journal paths, launch adapters, the GUI-facing protocol surface); the
// allow is scoped to THEM so ctl.rs itself stays under the strict lints of
// both binaries.
#[path = "../state.rs"]
#[allow(dead_code)]
mod state;
#[path = "../protocol.rs"]
#[allow(dead_code)]
mod protocol;
#[path = "../strip.rs"]
#[allow(dead_code)]
mod strip;
#[path = "../ctl.rs"]
mod ctl;

fn main() {
    std::process::exit(ctl::run(std::env::args().skip(1).collect()));
}
