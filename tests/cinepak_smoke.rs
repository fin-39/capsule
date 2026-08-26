#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use capsule::backend::capabilities::detect_with_environment_override;
use capsule::backend::launcher::build_launch_plan_with;
use capsule::model::{
    AudioPolicy, CapsuleRecord, Permissions, RunnerKind, StorageKind, WineVirtualDesktop,
};

/// Plays a caller-supplied, short Cinepak AVI through Wine's WinMM/MCI stack.
///
/// This is deliberately ignored: it opens a real Gamescope window, uses the
/// live audio bridge, and requires a local AVI supplied as
/// `CAPSULE_TEST_CINEPAK_AVI=/absolute/path/to/sample.avi`. No media fixture is
/// stored in the repository.
#[test]
#[ignore = "requires CAPSULE_TEST_CINEPAK_AVI and a live graphical Wayland session"]
fn plays_cinepak_through_contained_winmm_mci() {
    let supplied_avi = env::var_os("CAPSULE_TEST_CINEPAK_AVI").unwrap_or_else(|| {
        panic!(
            "set CAPSULE_TEST_CINEPAK_AVI to an existing short Cinepak AVI before running this ignored test"
        )
    });
    let supplied_avi = fs::canonicalize(&supplied_avi).unwrap_or_else(|error| {
        panic!(
            "could not resolve CAPSULE_TEST_CINEPAK_AVI {}: {error}",
            Path::new(&supplied_avi).display()
        )
    });
    assert!(
        supplied_avi.is_file(),
        "CAPSULE_TEST_CINEPAK_AVI is not a regular file: {}",
        supplied_avi.display()
    );

    let temp = tempfile::tempdir().unwrap();
    let prefix = temp.path().join("prefix");
    let drive_c = prefix.join("drive_c");
    fs::create_dir_all(&drive_c).unwrap();
    fs::copy(&supplied_avi, drive_c.join("cinepak-smoke.avi")).unwrap_or_else(|error| {
        panic!(
            "could not copy Cinepak input {} into the temporary capsule: {error}",
            supplied_avi.display()
        )
    });

    let source = temp.path().join("cinepak-smoke.c");
    fs::write(&source, CINEPAK_SMOKE_SOURCE).unwrap();
    let executable = drive_c.join("cinepak-smoke.exe");
    let compiler = Path::new("/usr/bin/x86_64-w64-mingw32-gcc");
    assert!(compiler.exists(), "Cinepak smoke test requires MinGW-w64");
    let compiled = Command::new(compiler)
        .args(["-Os", "-s", "-mwindows", "-o"])
        .arg(&executable)
        .arg(&source)
        .args(["-lwinmm", "-luser32", "-lgdi32"])
        .status()
        .unwrap();
    assert!(compiled.success(), "Cinepak MCI fixture failed to compile");

    let mut record = CapsuleRecord::new(
        "Cinepak MCI smoke test",
        StorageKind::DirectoryDev { path: prefix },
        "drive_c/cinepak-smoke.exe",
        RunnerKind::Wine,
    );
    record.permissions = Permissions::offline_game();
    // Capsule currently exposes its audio socket only through this explicit
    // combined playback/capture policy. The test itself only requests output.
    record.permissions.audio = AudioPolicy::PlaybackAndMicrophone;
    record.wine_virtual_desktop = Some(WineVirtualDesktop {
        width: 640,
        height: 480,
    });

    let capabilities = detect_with_environment_override().unwrap();
    let plan = build_launch_plan_with(&record, &capabilities).unwrap();
    let status = plan.command.execute().unwrap();
    let marker = record.storage.path().join("drive_c/cinepak-smoke-ran");
    assert!(
        marker.is_file(),
        "the contained fixture did not complete MCI playback and cleanup (status: {status})"
    );

    // Gamescope 3.16 can return a teardown failure on NVIDIA/Wayland after the
    // primary Wine client has already completed. The capsule-local marker is
    // the execution oracle, so preserve the status as diagnostics without
    // turning that known teardown fault into a false-negative playback test.
    if !status.success() {
        eprintln!("Cinepak MCI playback completed; helper chain returned {status}");
    }
}

const CINEPAK_SMOKE_SOURCE: &str = r#"
#include <windows.h>
#include <mmsystem.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static const char *CLASS_NAME = "CapsuleCinepakSmoke";
static const char *DEVICE = "capsuleclip";
static BOOL device_open = FALSE;
static BOOL playback_started = FALSE;
static int result_code = 20;
static ULONGLONG deadline = 0;

static MCIERROR send_mci(const char *command, char *reply, UINT reply_size, HWND callback) {
    return mciSendStringA(command, reply, reply_size, callback);
}

static MCIERROR close_device(void) {
    MCIERROR error = 0;
    if (device_open) {
        error = send_mci("close capsuleclip wait", NULL, 0, NULL);
        device_open = FALSE;
    }
    return error;
}

static BOOL write_marker(void) {
    HANDLE marker = CreateFileA(
        "C:\\cinepak-smoke-ran", GENERIC_WRITE, 0, NULL, CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL, NULL
    );
    if (marker == INVALID_HANDLE_VALUE) {
        return FALSE;
    }
    CloseHandle(marker);
    return TRUE;
}

static void resize_video(HWND window) {
    RECT client;
    char command[160];
    if (!device_open || !GetClientRect(window, &client)) {
        return;
    }
    snprintf(
        command, sizeof(command), "put %s destination at 0 0 %ld %ld",
        DEVICE, (long)(client.right - client.left), (long)(client.bottom - client.top)
    );
    send_mci(command, NULL, 0, NULL);
}

static void finish(HWND window, int code, BOOL completed) {
    MCIERROR close_error;
    KillTimer(window, 1);
    close_error = close_device();

    /* The success marker is deliberately created only after MCI reported a
       completed play operation and the AVI device closed successfully. */
    if (completed && close_error == 0 && write_marker()) {
        result_code = 0;
    } else {
        result_code = code;
    }
    DestroyWindow(window);
}

static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam) {
    (void)lparam;
    switch (message) {
        case WM_SIZE:
            resize_video(window);
            return 0;
        case MM_MCINOTIFY:
            if (playback_started && wparam == MCI_NOTIFY_SUCCESSFUL) {
                finish(window, 30, TRUE);
            } else if (playback_started) {
                finish(window, 31, FALSE);
            }
            return 0;
        case WM_TIMER:
            if (GetTickCount64() >= deadline) {
                finish(window, 32, FALSE);
            }
            return 0;
        case WM_CLOSE:
            finish(window, 33, FALSE);
            return 0;
        case WM_DESTROY:
            PostQuitMessage(result_code);
            return 0;
        default:
            return DefWindowProcA(window, message, wparam, lparam);
    }
}

int WINAPI WinMain(HINSTANCE instance, HINSTANCE previous, LPSTR command_line, int show) {
    WNDCLASSA window_class;
    HWND window;
    MSG message;
    char command[256];
    char length_reply[64];
    unsigned long long length_ms;
    ULONGLONG watchdog_ms;
    MCIERROR error;

    (void)previous;
    (void)command_line;
    ZeroMemory(&window_class, sizeof(window_class));
    window_class.lpfnWndProc = window_proc;
    window_class.hInstance = instance;
    window_class.hCursor = LoadCursorA(NULL, IDC_ARROW);
    window_class.hbrBackground = (HBRUSH)(COLOR_WINDOW + 1);
    window_class.lpszClassName = CLASS_NAME;
    if (!RegisterClassA(&window_class)) {
        return 10;
    }

    window = CreateWindowExA(
        0, CLASS_NAME, "Capsule Cinepak / WinMM MCI probe",
        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
        CW_USEDEFAULT, CW_USEDEFAULT, 640, 480,
        NULL, NULL, instance, NULL
    );
    if (!window) {
        return 11;
    }
    ShowWindow(window, show == 0 ? SW_SHOWNORMAL : show);
    UpdateWindow(window);

    snprintf(
        command, sizeof(command),
        "open C:\\cinepak-smoke.avi type avivideo alias %s style child parent %llu",
        DEVICE, (unsigned long long)(uintptr_t)window
    );
    error = send_mci(command, NULL, 0, window);
    if (error != 0) {
        finish(window, 12, FALSE);
    } else {
        device_open = TRUE;
        send_mci("set capsuleclip time format milliseconds wait", NULL, 0, NULL);
        resize_video(window);
        send_mci("window capsuleclip state show", NULL, 0, NULL);

        ZeroMemory(length_reply, sizeof(length_reply));
        error = send_mci(
            "status capsuleclip length wait", length_reply,
            (UINT)sizeof(length_reply), NULL
        );
        if (error != 0) {
            finish(window, 13, FALSE);
        } else {
            length_ms = strtoull(length_reply, NULL, 10);
            /* This probe is for short samples. Always terminate unattended,
               even when a broken codec never posts its completion notice. */
            watchdog_ms = (ULONGLONG)length_ms + 15000ULL;
            if (watchdog_ms < 30000ULL) {
                watchdog_ms = 30000ULL;
            }
            if (watchdog_ms > 120000ULL) {
                watchdog_ms = 120000ULL;
            }
            deadline = GetTickCount64() + watchdog_ms;
            if (!SetTimer(window, 1, 250, NULL)) {
                finish(window, 14, FALSE);
            } else {
                playback_started = TRUE;
                error = send_mci("play capsuleclip from 0 notify", NULL, 0, window);
                if (error != 0) {
                    finish(window, 15, FALSE);
                }
            }
        }
    }

    while (IsWindow(window) && GetMessageA(&message, NULL, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageA(&message);
    }
    return result_code;
}
"#;
