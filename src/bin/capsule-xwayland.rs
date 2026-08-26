//! Trusted Xwayland adapter for the Gamescope display.
//!
//! Gamescope normally enables MIT-SHM in its private Xwayland server. Wine's
//! X11 driver then tries to use System V shared memory, which cannot cross the
//! sandbox's private IPC namespace. Disabling the extension makes Wine use a
//! non-IPC transport while retaining Gamescope's per-run X server.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let Some(xwayland) = std::env::var_os("CAPSULE_REAL_XWAYLAND") else {
        eprintln!("CAPSULE_REAL_XWAYLAND is not set");
        return ExitCode::FAILURE;
    };
    if !Path::new(&xwayland).is_absolute() {
        eprintln!("CAPSULE_REAL_XWAYLAND is not an absolute path");
        return ExitCode::FAILURE;
    }

    let error = Command::new(xwayland)
        .arg("-extension")
        .arg("MIT-SHM")
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("could not start Xwayland: {error}");
    ExitCode::FAILURE
}
