#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;

use capsule::backend::capabilities::detect_with_environment_override;
use capsule::backend::launcher::{build_launch_plan_with, build_wine_prepare_plan_with};
use capsule::model::{
    AudioPolicy, CapsuleRecord, Permissions, RunnerKind, StorageKind, WineVirtualDesktop,
};

/// This opens a short-lived Gamescope window and therefore runs only when
/// requested explicitly from a graphical Wayland session.
#[test]
#[ignore = "requires a live Wayland session and user systemd manager"]
fn starts_a_wine_process_through_gamescope_and_sandwine() {
    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    // Match direct portable import: only the payload directory exists. Wine
    // must initialize all registry/prefix state after entering containment;
    // Capsule never runs wineboot or source executables on the host.
    fs::create_dir_all(prefix.join("drive_c")).unwrap();

    // A console-only `cmd /c exit` never creates a surface, which makes
    // Gamescope report a failed primary client even though Wine ran. Compile a
    // tiny GUI fixture that shows one private window briefly and exits on its
    // own, so this exercises the display boundary as well as Wine/Bubblewrap.
    let source = temp.path().join("capsule-smoke.c");
    fs::write(&source, GUI_SMOKE_SOURCE).unwrap();
    let executable = prefix.join("drive_c/capsule-smoke.exe");
    let compiler = Path::new("/usr/bin/x86_64-w64-mingw32-gcc");
    assert!(compiler.exists(), "runtime smoke test requires MinGW-w64");
    let compiled = Command::new(compiler)
        .args(["-Os", "-s", "-mwindows", "-o"])
        .arg(&executable)
        .arg(&source)
        .arg("-luser32")
        .status()
        .unwrap();
    assert!(compiled.success(), "GUI smoke fixture failed to compile");

    let mut record = CapsuleRecord::new(
        "Runtime smoke test",
        StorageKind::DirectoryDev { path: prefix },
        "drive_c/capsule-smoke.exe",
        RunnerKind::Wine,
    );
    record.permissions = Permissions::offline_game();
    record.permissions.audio = AudioPolicy::Off;
    // Exercise the compatibility path used by fixed-size Win9x-era games:
    // Wine owns one persistent desktop surface while the modal is opened and
    // closed inside it.
    record.wine_virtual_desktop = Some(WineVirtualDesktop {
        width: 640,
        height: 480,
    });

    let capabilities = detect_with_environment_override().unwrap();
    let preparation = build_wine_prepare_plan_with(&record, &capabilities)
        .unwrap()
        .expect("Wine preparation command");
    let preparation_status = preparation.execute().unwrap();
    assert!(
        preparation_status.success(),
        "headless Wine preparation returned {preparation_status}"
    );
    let plan = build_launch_plan_with(&record, &capabilities).unwrap();
    let status = plan.command.execute().unwrap();
    assert!(
        record
            .storage
            .path()
            .join("drive_c/capsule-smoke-ran")
            .is_file(),
        "the contained GUI fixture did not write its capsule-local marker (status: {status})"
    );
    // Gamescope 3.16 on the current NVIDIA/Wayland host can fail during
    // Xwayland teardown even after the GUI ran and exited cleanly. The
    // capsule-local marker is the contained-execution oracle; production still
    // surfaces a non-zero helper status instead of silently normalizing it.
    if !status.success() {
        eprintln!("runtime GUI completed; helper chain returned {status}");
    }
}

const GUI_SMOKE_SOURCE: &str = r#"
#include <windows.h>

static const char *TITLE = "Capsule runtime smoke";

static DWORD WINAPI close_dialog(LPVOID unused) {
    (void)unused;
    Sleep(1500);
    HWND window = FindWindowA(NULL, TITLE);
    if (window) {
        PostMessageA(window, WM_CLOSE, 0, 0);
    }
    return 0;
}

int WINAPI WinMain(HINSTANCE instance, HINSTANCE previous, LPSTR command_line, int show) {
    (void)instance;
    (void)previous;
    (void)command_line;
    (void)show;
    HANDLE closer = CreateThread(NULL, 0, close_dialog, NULL, 0, NULL);
    int result = MessageBoxA(NULL, "Private Wine/Xwayland test", TITLE, MB_OK);
    if (closer) {
        WaitForSingleObject(closer, 3000);
        CloseHandle(closer);
    }
    if (!result) {
        return 2;
    }

    HANDLE marker = CreateFileA(
        "C:\\capsule-smoke-ran", GENERIC_WRITE, 0, NULL, CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL, NULL
    );
    if (marker == INVALID_HANDLE_VALUE) {
        return 3;
    }
    CloseHandle(marker);
    return 0;
}
"#;
