//! Fail-closed launch planning for contained Wine and native applications.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Child;

use rustix::process::getuid;

use crate::backend::capabilities::{
    Capability, CapabilityError, CapabilityReport, detect_with_environment_override,
};
use crate::backend::{
    CommandSpec, PathValidationError, validate_capsule_relative, validate_host_absolute,
};
#[cfg(test)]
use crate::model::WineLocale;
use crate::model::{
    AudioPolicy, CapsuleRecord, NetworkPolicy, RunnerKind, StorageKind, WineGraphicsBackend,
};

// Gamescope otherwise defaults its private Xwayland screen to 1280x720. That
// is smaller than common 4:3 game surfaces such as 1280x960, so Gamescope
// constrains the top-level window and silently clips its bottom. A 1080p
// private canvas accommodates both those older windows (including native
// Wine menu chrome) and modern 16:9 games; `--scaler fit` maps the selected
// surface to the real compositor window without exposing the host desktop.
const DEFAULT_NESTED_WIDTH: u32 = 1920;
const DEFAULT_NESTED_HEIGHT: u32 = 1080;

// Wine normally discovers whatever fonts happen to be installed by the host
// distribution. That is not sufficient for older Windows software which
// checks for exact Windows family names (notably Japanese LiveMaker titles
// requiring `ＭＳ ゴシック`). Capsule ships Proton's redistributable
// compatibility fonts as a read-only runtime component instead. Bump this ID
// whenever the files or registration map changes so existing prefixes are
// migrated during their next hidden preparation phase.
const COMPATIBILITY_FONT_PACK_ID: &str = "proton-compat-2026-07-21-v1";
const COMPATIBILITY_FONT_FILES: &[&str] = &[
    "arial.ttf",
    "arialbd.ttf",
    "cour.ttf",
    "courbd.ttf",
    "georgia.ttf",
    "malgun.ttf",
    "micross.ttf",
    "msgothic.ttc",
    "msyh.ttf",
    "nirmala.ttf",
    "simsun.ttc",
    "times.ttf",
];
const INSTALLED_COMPATIBILITY_FONT_DIR: &str = "/usr/share/capsule/fonts/windows-compat";

// DXVK replaces Wine's CPU-heavy Direct3D-to-OpenGL path with a
// Direct3D-to-Vulkan implementation. Keep it as one immutable shared runtime
// component: prefixes contain only symlinks, so enabling it does not duplicate
// the DLL payload in every sparse capsule image.
const DXVK_PACK_ID: &str = "dxvk-2.7.1";
const DXVK_DLL_FILES: &[&str] = &[
    "d3d8.dll",
    "d3d9.dll",
    "d3d10core.dll",
    "d3d11.dll",
    "dxgi.dll",
];
const INSTALLED_DXVK_DIR: &str = "/usr/share/capsule/dxvk/windows-compat";
const WINE_DXVK_OVERRIDES: &str =
    "WINEDLLOVERRIDES=d3d8,d3d9,d3d10core,d3d11,dxgi=n,b;mscoree=d;mshtml=d;winemenubuilder.exe=d";
const WINE_D3D_OVERRIDES: &str =
    "WINEDLLOVERRIDES=d3d8,d3d9,d3d10core,d3d11,dxgi=b;mscoree=d;mshtml=d;winemenubuilder.exe=d";

#[derive(Clone, Debug)]
pub struct LaunchPlan {
    pub command: CommandSpec,
    pub warnings: Vec<String>,
}

impl LaunchPlan {
    pub fn spawn(&self) -> io::Result<Child> {
        self.command.spawn()
    }
}

/// Build a launch plan without starting any process.
///
/// Single-file images require the supervisor to mount them first, so this
/// initial convenience entry point supports development directories only. It
/// still uses the complete sandbox chain and never falls back to bare Wine.
pub fn build_launch_plan(record: &CapsuleRecord) -> Result<LaunchPlan, LaunchError> {
    let capabilities = detect_with_environment_override()?;
    build_launch_plan_with(record, &capabilities)
}

pub fn build_launch_plan_with(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
) -> Result<LaunchPlan, LaunchError> {
    build_launch_plan_with_status(record, capabilities, None)
}

pub(crate) fn build_launch_plan_with_status(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
    trusted_status_path: Option<&Path>,
) -> Result<LaunchPlan, LaunchError> {
    build_launch_plan_with_runtime(record, capabilities, trusted_status_path, None, false)
}

pub(crate) fn build_launch_plan_with_status_and_playback_socket(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
    trusted_status_path: Option<&Path>,
    playback_audio_socket: Option<&Path>,
) -> Result<LaunchPlan, LaunchError> {
    build_launch_plan_with_runtime(
        record,
        capabilities,
        trusted_status_path,
        playback_audio_socket,
        false,
    )
}

/// Build a launch plan for a trusted in-capsule utility such as the Steam
/// installer or Steam client. These programs commonly replace their initial
/// process while leaving child processes in the Wine server, so the utility
/// plan waits for the complete Wine session instead of terminating it when
/// the first process exits.
#[cfg(test)]
pub(crate) fn build_wine_utility_launch_plan_with_status(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
    trusted_status_path: Option<&Path>,
) -> Result<LaunchPlan, LaunchError> {
    build_launch_plan_with_runtime(record, capabilities, trusted_status_path, None, true)
}

pub(crate) fn build_wine_utility_launch_plan_with_status_and_playback_socket(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
    trusted_status_path: Option<&Path>,
    playback_audio_socket: Option<&Path>,
) -> Result<LaunchPlan, LaunchError> {
    build_launch_plan_with_runtime(
        record,
        capabilities,
        trusted_status_path,
        playback_audio_socket,
        true,
    )
}

/// Build the display-less Wine prefix preparation command used immediately
/// before the visible game launch.
///
/// Wine updates an existing prefix when the installed `wine.inf` revision
/// changes. Running this as a separate, equally-contained phase prevents that
/// work from becoming the first visible Wine window and guarantees it has
/// fully stopped before Gamescope and the game are started.
pub fn build_wine_prepare_plan_with(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
) -> Result<Option<CommandSpec>, LaunchError> {
    record.validate().map_err(LaunchError::InvalidRecord)?;
    if record.runner != RunnerKind::Wine {
        return Ok(None);
    }

    let root = match &record.storage {
        StorageKind::DirectoryDev { path } => path,
        StorageKind::Image { .. } | StorageKind::ExternalImage { .. } => {
            return Err(LaunchError::ImageSupervisorRequired);
        }
    };
    validate_runtime_root(root)?;

    let systemd_run = require(capabilities, Capability::SystemdRun)?;
    let sandwine = require(capabilities, Capability::Sandwine)?;
    require(capabilities, Capability::Bubblewrap)?;
    let wine_tools = WineTools::from_capabilities(capabilities)?;
    let bundled_runtime = BundledRuntime::from_environment()?;

    let capsule_home = root.join(".capsule-home");
    let compatibility_fonts = compatibility_font_dir()?;
    ensure_font_pack_outside_root(&compatibility_fonts, root)?;
    // Prefixes keep architecture-correct symlinks into the shared DXVK pack.
    // Keep that immutable directory mounted during preparation even when this
    // run forces WineD3D; switching graphics backends must never turn those
    // managed links into dangling Wine system DLLs.
    let dxvk = dxvk_dir()
        .and_then(|path| {
            ensure_dxvk_outside_root(&path, root)?;
            Ok(path)
        })
        .ok()
        .filter(|path| !path.starts_with(root));
    let mut command = CommandSpec::new(systemd_run)
        .arg("--user")
        .arg("--collect")
        .arg("--wait")
        .arg("--pipe")
        .arg("--quiet")
        .arg("--expand-environment=no")
        .arg(format!("--unit=capsule-prepare-{}", record.id.simple()))
        // Preparation must never create a desktop window. Sandwine also gets
        // no X11/Wayland socket, but clearing these values makes that boundary
        // explicit and fail-closed if its defaults ever change.
        .arg("--setenv=DISPLAY=")
        .arg("--setenv=WAYLAND_DISPLAY=")
        .arg("--setenv=XAUTHORITY=")
        .arg("--setenv=DBUS_SESSION_BUS_ADDRESS=")
        .arg("--property=LimitCORE=0");
    command = add_systemd_runtime_environment(command, bundled_runtime.as_ref());
    if let Some(memory) = record.permissions.memory_limit_mib {
        command = command.arg(format!("--property=MemoryMax={memory}M"));
    }
    if let Some(processes) = record.permissions.process_limit {
        command = command.arg(format!("--property=TasksMax={processes}"));
    }

    command = command.arg(sandwine.as_os_str());
    command = add_sandwine_runtime(command, bundled_runtime.as_ref());
    command = add_wine_tools(command, &wine_tools);
    command = command
        .arg("--pass")
        .arg(read_write_mount_argument(root))
        .arg("--pass")
        .arg(read_only_mount_argument(&compatibility_fonts))
        .arg("--env")
        .arg(path_environment("CAPSULE_ROOT", root))
        .arg("--env")
        .arg(path_environment("HOME", &capsule_home))
        .arg("--env")
        .arg(path_environment(
            "XDG_CACHE_HOME",
            &capsule_home.join("cache"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_CONFIG_HOME",
            &capsule_home.join("config"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_DATA_HOME",
            &capsule_home.join("data"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_STATE_HOME",
            &capsule_home.join("state"),
        ))
        .arg("--env")
        .arg("USER=capsule")
        .arg("--env")
        .arg("LOGNAME=capsule")
        .arg("--env")
        .arg(path_environment("WINEPREFIX", root))
        .arg("--env")
        .arg(path_environment(
            "CAPSULE_COMPAT_FONTS",
            &compatibility_fonts,
        ))
        .arg("--env")
        .arg(format!(
            "CAPSULE_COMPAT_FONTS_ID={COMPATIBILITY_FONT_PACK_ID}"
        ));
    if let Some(dxvk) = &dxvk {
        command = command
            .arg("--pass")
            .arg(read_only_mount_argument(dxvk))
            .arg("--env")
            .arg(path_environment("CAPSULE_DXVK_DIR", dxvk))
            .arg("--env")
            .arg(format!("CAPSULE_DXVK_ID={DXVK_PACK_ID}"));
    }
    command = command
        .arg("--env")
        .arg(wine_debug_environment())
        .arg("--env")
        .arg("WINEDLLOVERRIDES=mscoree=d;mshtml=d;winemenubuilder.exe=d")
        .arg("--env")
        .arg("LANG=C.UTF-8")
        .arg("--env")
        .arg("SHELL=/bin/sh")
        .arg("--no-wine")
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(SANDBOX_WINE_PREPARE_SCRIPT)
        .arg("capsule-wine-prepare");

    Ok(Some(command))
}

fn build_launch_plan_with_runtime(
    record: &CapsuleRecord,
    capabilities: &CapabilityReport,
    trusted_status_path: Option<&Path>,
    playback_audio_socket_override: Option<&Path>,
    wait_for_wine_server: bool,
) -> Result<LaunchPlan, LaunchError> {
    record.validate().map_err(LaunchError::InvalidRecord)?;

    if matches!(
        record.permissions.network,
        NetworkPolicy::LanOnly | NetworkPolicy::Custom { .. }
    ) {
        return Err(LaunchError::FilteredNetworkUnavailable);
    }
    if record.permissions.controllers {
        return Err(LaunchError::ControllerBrokerUnavailable);
    }
    if !record.permissions.gpu {
        // Sandwine's private-display integration currently mounts /dev/dri.
        // Pretending that the GPU is disabled would violate the displayed
        // permission.
        return Err(LaunchError::SoftwareDisplayUnavailable);
    }

    let root = match &record.storage {
        StorageKind::DirectoryDev { path } => path,
        StorageKind::Image { .. } | StorageKind::ExternalImage { .. } => {
            return Err(LaunchError::ImageSupervisorRequired);
        }
    };
    validate_runtime_root(root)?;
    let mut warnings = Vec::new();
    if record.permissions.network == NetworkPolicy::Lan {
        warnings.push(
            "Internet + LAN shares the host network namespace; local services and listening ports are exposed."
                .into(),
        );
    }
    let compatibility_fonts = if record.runner == RunnerKind::Wine {
        let path = compatibility_font_dir()?;
        ensure_font_pack_outside_root(&path, root)?;
        Some(path)
    } else {
        None
    };
    let dxvk = if record.runner == RunnerKind::Wine {
        match dxvk_dir().and_then(|path| {
            ensure_dxvk_outside_root(&path, root)?;
            Ok(path)
        }) {
            Ok(path) => Some(path),
            Err(error) => {
                if record.wine_graphics_backend == WineGraphicsBackend::Dxvk {
                    warnings.push(format!(
                        "DXVK is unavailable, so this run is using WineD3D: {error}"
                    ));
                }
                None
            }
        }
    } else {
        None
    };
    let use_dxvk = record.wine_graphics_backend == WineGraphicsBackend::Dxvk && dxvk.is_some();
    if let Some(status_path) = trusted_status_path {
        validate_host_absolute(status_path)?;
        if status_path.starts_with(root) {
            return Err(LaunchError::UnsafeStatusPath(status_path.to_path_buf()));
        }
    }
    validate_capsule_relative(&record.entrypoint)?;
    if let Some(working_dir) = &record.working_dir {
        validate_capsule_relative(working_dir)?;
    }
    let playback_audio_socket = if matches!(record.permissions.audio, AudioPolicy::PlaybackOnly) {
        let path = playback_audio_socket_override
            .map(Path::to_path_buf)
            .unwrap_or_else(default_playback_audio_socket);
        validate_playback_audio_socket(&path)?;
        Some(path)
    } else {
        None
    };

    let systemd_run = require(capabilities, Capability::SystemdRun)?;
    let gamescope = require(capabilities, Capability::Gamescope)?;
    let xwayland = require(capabilities, Capability::Xwayland)?;
    let xwayland_wrapper = require(capabilities, Capability::XwaylandWrapper)?;
    let window_center = ((record.runner == RunnerKind::Wine
        && record.wine_virtual_desktop.is_some())
        || record.permissions.clipboard)
        .then(|| require(capabilities, Capability::WindowCenter))
        .transpose()?;
    let clipboard_guard = (!record.permissions.clipboard)
        .then(|| require(capabilities, Capability::ClipboardGuard))
        .transpose()?;
    let wl_paste = record
        .permissions
        .clipboard
        .then(|| require(capabilities, Capability::WlPaste))
        .transpose()?;
    let sandwine = require(capabilities, Capability::Sandwine)?;
    let bundled_runtime = BundledRuntime::from_environment()?;
    let wine_tools = (record.runner == RunnerKind::Wine)
        .then(|| WineTools::from_capabilities(capabilities))
        .transpose()?;
    let internet_only_network = if record.permissions.network == NetworkPolicy::InternetOnly {
        Some((
            require(capabilities, Capability::NetworkHelper)?,
            require(capabilities, Capability::Slirp4netns)?,
            require(capabilities, Capability::Nft)?,
        ))
    } else {
        None
    };
    require(capabilities, Capability::Bubblewrap)?;
    let mut command = CommandSpec::new(systemd_run)
        .arg("--user")
        .arg("--collect")
        .arg("--wait")
        .arg("--pipe")
        .arg("--quiet")
        // The launch scripts are passed as literal argv entries and must be
        // expanded only by their own /bin/sh processes. systemd-run otherwise
        // rewrites constructs such as `$$` and `${value%%pattern}` while it is
        // creating the transient unit, corrupting the window helper PID and
        // locale names before the sandbox starts.
        .arg("--expand-environment=no")
        .arg(format!("--unit=capsule-{}", record.id.simple()))
        // Gamescope replaces this with its per-run Xwayland display for the
        // child. If that handoff ever fails, the nonexistent sentinel makes
        // Sandwine fail instead of silently selecting the desktop's :0.
        .arg("--setenv=DISPLAY=:99999")
        .arg("--setenv=XAUTHORITY=")
        .arg("--setenv=DBUS_SESSION_BUS_ADDRESS=")
        .arg("--setenv=SSH_AUTH_SOCK=")
        .arg("--setenv=GPG_AGENT_INFO=")
        .arg(systemd_path_environment("WLR_XWAYLAND", &xwayland_wrapper))
        .arg(systemd_path_environment("CAPSULE_REAL_XWAYLAND", &xwayland))
        // Gamescope 3.16 can fault during Xwayland teardown on some NVIDIA
        // hosts. Never write a core image containing process memory.
        .arg("--property=LimitCORE=0")
        // Sandwine resolves Bubblewrap and a few POSIX helpers through PATH.
        // Use only the immutable AppImage runtime (when present) and trusted
        // system directories, never the desktop user's arbitrary PATH.
        ;
    command = add_systemd_runtime_environment(command, bundled_runtime.as_ref());
    if wait_for_wine_server {
        // Steam's installer and Chromium-based client windows can fail to
        // acquire a presentable Xwayland pixmap on NVIDIA/Optimus systems.
        // These trusted setup utilities do not need accelerated X11: let
        // Xwayland use its software path for this transient unit only. Game
        // launches keep Glamor/DRI3 enabled so DXVK can present efficiently.
        command = command.arg("--setenv=XWAYLAND_NO_GLAMOR=1");
    }
    // Keep the host compositor and driver discovery state available to the
    // trusted outer Gamescope process. Driver binaries themselves stay on the
    // host because they must match the running kernel and hardware.
    for variable in [
        "XDG_RUNTIME_DIR",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "VK_DRIVER_FILES",
        "VK_ICD_FILENAMES",
        "__EGL_VENDOR_LIBRARY_DIRS",
        "LIBGL_DRIVERS_PATH",
        "GBM_BACKENDS_PATH",
        "__GLX_VENDOR_LIBRARY_NAME",
    ] {
        if let Some(value) = std::env::var_os(variable) {
            let mut assignment = OsString::from(format!("--setenv={variable}="));
            assignment.push(value);
            command = command.arg(assignment);
        }
    }
    if let Some(memory) = record.permissions.memory_limit_mib {
        command = command.arg(format!("--property=MemoryMax={memory}M"));
    }
    if let Some(processes) = record.permissions.process_limit {
        command = command.arg(format!("--property=TasksMax={processes}"));
    }

    let (nested_width, nested_height) = gamescope_nested_size(record);

    if let Some(guard) = clipboard_guard {
        // Gamescope's Wayland backend otherwise advertises private X11 text
        // selections to the host compositor. Hide the two host selection
        // protocols when Clipboard is off. The child wrapper below removes
        // the interposer before Sandwine starts.
        command = command
            .arg("/usr/bin/env")
            .arg(format!("LD_PRELOAD={}", guard.display()))
            .arg("CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD=1");
    }

    command = command
        .arg(gamescope.as_os_str())
        // This avoids a known NVIDIA NVVM compute-pipeline failure on the
        // current host while preserving the private nested compositor.
        .arg("--disable-color-management")
        .arg("--backend")
        .arg("wayland")
        .arg("--xwayland-count")
        .arg("1")
        // Keep the complete Win32 top-level window inside the private screen.
        // Gamescope's 1280x720 default clips taller fixed 4:3 clients before
        // its fit scaler sees them, making their bottom controls unreachable.
        // A Wine virtual desktop instead defines the complete surface size
        // and therefore maps one-to-one to the nested display.
        .arg("--nested-width")
        .arg(nested_width.to_string())
        .arg("--nested-height")
        .arg(nested_height.to_string())
        // Let Gamescope scale the selected surface while preserving its
        // aspect ratio. Resizing every inner window to the nested display
        // breaks fixed-size GDI games that continue painting only their
        // original client area.
        .arg("--scaler")
        .arg("fit")
        .arg("--");

    if let Some(status_path) = trusted_status_path {
        // This wrapper stays outside Sandwine's namespaces. The contained app
        // cannot access the supervisor-owned status path, so a clean child
        // exit can be distinguished from Gamescope crashing during teardown.
        command = command
            .arg("/bin/sh")
            .arg("-c")
            .arg(TRUSTED_STATUS_LAUNCH_SCRIPT)
            .arg("capsule-status")
            .arg(status_path.as_os_str());
    }

    // Gamescope supplies its private DISPLAY to this fixed wrapper. It
    // removes the host-only clipboard guard before entering Sandwine and, if
    // requested, starts the trusted private-display sidecar for window
    // centering and/or bounded text clipboard import.
    let (desktop_width, desktop_height) = record
        .wine_virtual_desktop
        .map(|desktop| (desktop.width, desktop.height))
        .unwrap_or((0, 0));
    command = command
        .arg("/bin/sh")
        .arg("-c")
        .arg(PRIVATE_DISPLAY_LAUNCH_SCRIPT)
        .arg("capsule-private-display")
        .arg(window_center.as_deref().unwrap_or_else(|| Path::new("")))
        .arg(desktop_width.to_string())
        .arg(desktop_height.to_string())
        .arg(if record.permissions.clipboard {
            "1"
        } else {
            "0"
        })
        .arg(wl_paste.as_deref().unwrap_or_else(|| Path::new("")));

    if let Some((network_helper, slirp4netns, nft)) = internet_only_network {
        command = command
            .arg(network_helper.as_os_str())
            .arg("--slirp4netns")
            .arg(slirp4netns.as_os_str())
            .arg("--nft")
            .arg(nft.as_os_str())
            .arg("--")
            .arg(sandwine.as_os_str());
    } else {
        command = command.arg(sandwine.as_os_str());
    }
    if let Some(wine_tools) = &wine_tools {
        command = add_sandwine_runtime(command, bundled_runtime.as_ref());
        command = add_wine_tools(command, wine_tools);
    }

    // Bind the capsule root at its supervisor-owned runtime path. Both native
    // and Wine applications receive a private persistent home below that root;
    // no path from the desktop user's real home is available in the sandbox.
    let capsule_home = root.join(".capsule-home");
    command = command
        .arg("--pass")
        .arg(read_write_mount_argument(root))
        .arg("--env")
        .arg(path_environment("CAPSULE_ROOT", root))
        .arg("--env")
        .arg(path_environment("HOME", &capsule_home))
        .arg("--env")
        .arg(path_environment(
            "XDG_CACHE_HOME",
            &capsule_home.join("cache"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_CONFIG_HOME",
            &capsule_home.join("config"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_DATA_HOME",
            &capsule_home.join("data"),
        ))
        .arg("--env")
        .arg(path_environment(
            "XDG_STATE_HOME",
            &capsule_home.join("state"),
        ))
        .arg("--env")
        .arg("USER=capsule")
        .arg("--env")
        .arg("LOGNAME=capsule")
        // systemd-run cleared the desktop's DISPLAY above. Gamescope now sets
        // DISPLAY to its own per-run Xwayland server before starting Sandwine.
        // Sandwine's unfortunately named host-X11 option therefore binds only
        // that nested socket, never the desktop X11 socket. Wine's X11 driver
        // is still substantially more compatible with Win9x-era games than
        // its native Wayland driver.
        .arg("--host-x11-danger-danger")
        // Capsule invokes the selected runner explicitly below after setting
        // its contained working directory.
        .arg("--no-wine")
        .arg("--env")
        .arg("LANG=C.UTF-8")
        // Sandwine's PTY helper uses `$SHELL` to execute its safely quoted
        // command. Pin it to the POSIX shell Sandwine's quoting targets.
        .arg("--env")
        .arg("SHELL=/bin/sh");

    if !matches!(record.permissions.network, NetworkPolicy::Off) {
        command = command.arg("--network");
    }

    if let Some(fonts) = &compatibility_fonts {
        // Only this public, immutable asset directory is added. The game does
        // not receive the Proton installation or the rest of Capsule's source
        // tree, and the shared pack cannot be modified from inside the sandbox.
        command = command.arg("--pass").arg(read_only_mount_argument(fonts));
    }

    if let Some(dxvk) = &dxvk {
        command = command.arg("--pass").arg(read_only_mount_argument(dxvk));
    }

    if record.runner == RunnerKind::Wine {
        command = command
            .arg("--env")
            .arg(path_environment("WINEPREFIX", root))
            .arg("--env")
            .arg(wine_debug_environment())
            .arg("--env")
            .arg(if use_dxvk {
                WINE_DXVK_OVERRIDES
            } else {
                WINE_D3D_OVERRIDES
            });
        if record.wine_steam {
            command = command.arg("--env").arg("CAPSULE_START_STEAM=1");
        }
        if wait_for_wine_server {
            command = command.arg("--env").arg("CAPSULE_WAIT_FOR_WINESERVER=1");
        }
    }

    if Path::new("/dev/nvidiactl").exists() {
        command = command.arg("--nvidia-gpu");
    }

    match record.permissions.audio {
        AudioPolicy::Off => {}
        AudioPolicy::PlaybackOnly => {
            let socket = playback_audio_socket
                .as_deref()
                .expect("playback socket was validated above");
            let mut mount = socket.as_os_str().to_os_string();
            mount.push(":ro");
            let mut server = OsString::from("PULSE_SERVER=unix:");
            server.push(socket.as_os_str());
            command = command.arg("--pass").arg(mount).arg("--env").arg(server);
        }
        AudioPolicy::PlaybackAndMicrophone => {
            command = command.arg("--pulseaudio");
            warnings.push(
                "The host PulseAudio socket permits playback and recording; microphone access is enabled."
                    .into(),
            );
        }
    }

    let working_dir = record.working_dir.as_deref().unwrap_or_else(|| {
        record
            .entrypoint
            .parent()
            .unwrap_or_else(|| Path::new("drive_c"))
    });
    ensure_inside_drive_c(working_dir)?;
    ensure_inside_drive_c(&record.entrypoint)?;

    // Keep every record-controlled value in its own positional argument. The
    // fixed shell fragment expands only quoted positional parameters, so path
    // characters and application arguments are never parsed as shell syntax.
    command = match record.runner {
        RunnerKind::Wine => {
            let virtual_desktop_size = record
                .wine_virtual_desktop
                .map(|desktop| format!("{}x{}", desktop.width, desktop.height))
                .unwrap_or_default();
            command
                .arg("--")
                .arg("sh")
                .arg("-c")
                .arg(SANDBOX_WINE_LAUNCH_SCRIPT)
                .arg("capsule-wine-launch")
                .arg(record.wine_locale.id())
                .arg(virtual_desktop_size)
                .arg(working_dir.as_os_str())
                .arg(record.entrypoint.as_os_str())
                .args(record.arguments.iter().map(OsString::from))
        }
        RunnerKind::Native => command
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg(SANDBOX_NATIVE_LAUNCH_SCRIPT)
            .arg("capsule-native-launch")
            .arg(working_dir.as_os_str())
            .arg(record.entrypoint.as_os_str())
            .args(record.arguments.iter().map(OsString::from)),
    };

    Ok(LaunchPlan { command, warnings })
}

fn gamescope_nested_size(record: &CapsuleRecord) -> (u32, u32) {
    match (record.runner, record.wine_virtual_desktop) {
        (RunnerKind::Wine, Some(desktop)) => (desktop.width, desktop.height),
        _ => (DEFAULT_NESTED_WIDTH, DEFAULT_NESTED_HEIGHT),
    }
}

const PRIVATE_DISPLAY_LAUNCH_SCRIPT: &str = r#"helper=$1
width=$2
height=$3
clipboard=$4
wl_paste=$5
shift 5
unset LD_PRELOAD CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD
if [ -n "$helper" ]; then
    "$helper" "$width" "$height" "$$" "$clipboard" "$wl_paste" &
fi
exec "$@""#;

const TRUSTED_STATUS_LAUNCH_SCRIPT: &str = r#"status_file=$1
shift
"$@"
status=$?
umask 077
printf '%s\n' "$status" > "$status_file"
exit "$status""#;

const SANDBOX_WINE_PREPARE_SCRIPT: &str = r#"prefix=${WINEPREFIX:?}
wine=${CAPSULE_WINE:?}
wineserver=${CAPSULE_WINESERVER:?}
wineboot=${CAPSULE_WINEBOOT:?}
winepath=${CAPSULE_WINEPATH:?}
wine_inf=${CAPSULE_WINE_INF:?}
contained_home=${HOME:?}
font_dir=${CAPSULE_COMPAT_FONTS:?}
font_pack_id=${CAPSULE_COMPAT_FONTS_ID:?}
font_marker=$prefix/.capsule-compat-fonts
dxvk_dir=${CAPSULE_DXVK_DIR:-}
dxvk_pack_id=${CAPSULE_DXVK_ID:-}
dxvk_marker=$prefix/.capsule-dxvk
/usr/bin/mkdir -p -- \
    "$contained_home" \
    "${XDG_CACHE_HOME:?}" \
    "${XDG_CONFIG_HOME:?}" \
    "${XDG_DATA_HOME:?}" \
    "${XDG_STATE_HOME:?}" || exit 125
unset DISPLAY WAYLAND_DISPLAY XAUTHORITY DBUS_SESSION_BUS_ADDRESS
ready=$prefix/.capsule-wine-ready
system32=$prefix/drive_c/windows/system32
wine_revision=$(/usr/bin/stat -c %Y -- "$wine_inf" 2>/dev/null) || wine_revision=
prefix_revision=
if [ -f "$prefix/.update-timestamp" ]; then
    # Wine writes this text file through Win32 and therefore uses CRLF.
    prefix_revision=$(/usr/bin/tr -d '\r\n' < "$prefix/.update-timestamp") || prefix_revision=
fi
case "$prefix_revision" in
    ''|*[!0-9]*) prefix_revision= ;;
esac
prefix_valid=0
if [ -s "$prefix/system.reg" ] &&
   [ -s "$system32/shell32.dll" ] &&
   [ -s "$system32/user32.dll" ] &&
   [ -s "$system32/ucrtbase.dll" ]; then
    prefix_valid=1
fi
if [ "$prefix_valid" -eq 0 ]; then
    # A compositor or host crash can interrupt Wine's first prefix setup.
    # Remove only Wine-generated state; imported content always lives in
    # drive_c/Game and is deliberately preserved.
    "$wineserver" -k >/dev/null 2>&1 || :
    "$wineserver" -w >/dev/null 2>&1 || :
    /usr/bin/rm -rf -- \
        "$prefix/dosdevices" \
        "$prefix/system.reg" \
        "$prefix/user.reg" \
        "$prefix/userdef.reg" \
        "$ready" \
        "$font_marker" \
        "$prefix/.update-timestamp" \
        "$prefix/drive_c/Program Files" \
        "$prefix/drive_c/Program Files (x86)" \
        "$prefix/drive_c/ProgramData" \
        "$prefix/drive_c/users" \
        "$prefix/drive_c/windows"
fi
prefix_refreshed=0
if [ "$prefix_valid" -eq 0 ] ||
   [ -z "$wine_revision" ] ||
   [ "$prefix_revision" != "$wine_revision" ]; then
    # Wine may replace its system DLLs during a prefix upgrade. Remove only
    # symlinked DXVK entries first so wineboot cannot accidentally try to
    # update Capsule's read-only shared assets through those links.
    for dxvk_system_dir in \
        "$prefix/drive_c/windows/system32" \
        "$prefix/drive_c/windows/syswow64"; do
        for dxvk_dll in d3d8.dll d3d9.dll d3d10core.dll d3d11.dll dxgi.dll; do
            if [ -L "$dxvk_system_dir/$dxvk_dll" ]; then
                /usr/bin/rm -f -- "$dxvk_system_dir/$dxvk_dll" || exit 125
            fi
        done
    done
    "$wineboot" --init
    boot_status=$?
    prefix_refreshed=1
    # wineboot can return while background services still own the prefix.
    # Stop that preparation-only server and wait for every write before the
    # visible game phase starts a fresh one on Gamescope's private display.
    "$wineserver" -k >/dev/null 2>&1 || :
    "$wineserver" -w >/dev/null 2>&1
    wait_status=$?
    if [ ! -s "$prefix/system.reg" ] ||
       [ ! -s "$system32/shell32.dll" ] ||
       [ ! -s "$system32/user32.dll" ] ||
       [ ! -s "$system32/ucrtbase.dll" ]; then
        if [ "$boot_status" -eq 0 ]; then boot_status=126; fi
        exit "$boot_status"
    fi
    if [ "$wait_status" -ne 0 ]; then
        exit "$wait_status"
    fi
    if [ "$boot_status" -ne 0 ]; then
        printf 'Capsule: wineboot returned %s after completing the verified prefix\n' "$boot_status" >&2
    fi
fi

# Make Capsule's shared compatibility fonts part of this prefix without
# copying them into every image. The assets are mounted read-only and the
# visible game phase receives the same narrow mount. Storing both the pack ID
# and its host location makes development-to-installed-path moves migrate
# cleanly instead of leaving stale Z: registry paths behind.
marker_pack_id=
marker_font_dir=
if [ -f "$font_marker" ]; then
    marker_pack_id=$(/usr/bin/sed -n '1p' "$font_marker") || marker_pack_id=
    marker_font_dir=$(/usr/bin/sed -n '2p' "$font_marker") || marker_font_dir=
fi
if [ "$prefix_refreshed" -eq 1 ] ||
   [ "$marker_pack_id" != "$font_pack_id" ] ||
   [ "$marker_font_dir" != "$font_dir" ]; then
    windows_font_dir=$("$winepath" -w "$font_dir")
    registration_status=$?
    windows_font_dir=$(printf '%s' "$windows_font_dir" | /usr/bin/tr -d '\r\n')
    if [ "$registration_status" -eq 0 ] && [ -z "$windows_font_dir" ]; then
        registration_status=125
    fi

    font_key='HKLM\Software\Microsoft\Windows\CurrentVersion\Fonts'
    font_nt_key='HKLM\Software\Microsoft\Windows NT\CurrentVersion\Fonts'
    register_font() {
        font_file=$1
        shift
        [ -s "$font_dir/$font_file" ] || return 125
        windows_font_file="${windows_font_dir}\\${font_file}"
        for font_family in "$@"; do
            font_value="$font_family (TrueType)"
            "$wine" reg add "$font_key" /v "$font_value" /t REG_SZ /d "$windows_font_file" /f || return
            "$wine" reg add "$font_nt_key" /v "$font_value" /t REG_SZ /d "$windows_font_file" /f || return
        done
    }
    register_all_fonts() {
        register_font arial.ttf 'Arial' || return
        register_font arialbd.ttf 'Arial Bold' || return
        register_font cour.ttf 'Courier New' || return
        register_font courbd.ttf 'Courier New Bold' || return
        register_font georgia.ttf 'Georgia' || return
        register_font times.ttf 'Times New Roman' || return
        register_font malgun.ttf 'Malgun Gothic' || return
        register_font micross.ttf 'Microsoft Sans Serif' || return
        register_font msgothic.ttc 'MS Gothic' 'MS PGothic' 'MS UI Gothic' || return
        register_font msyh.ttf 'Microsoft YaHei' || return
        register_font nirmala.ttf 'Nirmala UI' || return
        register_font simsun.ttc 'SimSun' 'NSimSun' || return
    }
    if [ "$registration_status" -eq 0 ]; then
        register_all_fonts
        registration_status=$?
    fi
    "$wineserver" -k >/dev/null 2>&1 || :
    "$wineserver" -w >/dev/null 2>&1
    registration_wait_status=$?
    if [ "$registration_status" -ne 0 ]; then
        exit "$registration_status"
    fi
    if [ "$registration_wait_status" -ne 0 ]; then
        exit "$registration_wait_status"
    fi
    printf '%s\n%s\n' "$font_pack_id" "$font_dir" > "$font_marker" || exit 125
fi

# Install DXVK by symlink, following the same architecture mapping recommended
# upstream: 64-bit DLLs in system32 and 32-bit DLLs in syswow64 for a win64
# prefix, or 32-bit DLLs in system32 for a win32 prefix. The shared source is
# mounted read-only at the same absolute path during the visible game phase.
# WineD3D compatibility mode simply forces built-ins and ignores these links.
if [ -n "$dxvk_dir" ]; then
    [ -n "$dxvk_pack_id" ] || exit 125
    prefix_arch=$(/usr/bin/sed -n '/^#arch=/{s/^#arch=//;p;q;}' "$prefix/system.reg")
    install_dxvk_arch() {
        dxvk_target_dir=$1
        dxvk_asset_arch=$2
        /usr/bin/mkdir -p -- "$dxvk_target_dir" || return
        for dxvk_dll in d3d8.dll d3d9.dll d3d10core.dll d3d11.dll dxgi.dll; do
            dxvk_asset=$dxvk_dir/$dxvk_asset_arch/$dxvk_dll
            [ -s "$dxvk_asset" ] || return 125
            /usr/bin/ln -sfn -- "$dxvk_asset" "$dxvk_target_dir/$dxvk_dll" || return
        done
    }
    case "$prefix_arch" in
        win64)
            install_dxvk_arch "$prefix/drive_c/windows/system32" x64 || exit
            install_dxvk_arch "$prefix/drive_c/windows/syswow64" x32 || exit
            ;;
        win32)
            install_dxvk_arch "$prefix/drive_c/windows/system32" x32 || exit
            ;;
        *)
            printf 'Capsule: unsupported Wine prefix architecture: %s\n' "$prefix_arch" >&2
            exit 125
            ;;
    esac
    printf '%s\n%s\n%s\n' \
        "$dxvk_pack_id" "$dxvk_dir" "$prefix_arch" > "$dxvk_marker" || exit 125
fi
: > "$ready"
exit 0"#;

const SANDBOX_WINE_LAUNCH_SCRIPT: &str = r#"prefix=${WINEPREFIX:?}
wine=${CAPSULE_WINE:?}
wineserver=${CAPSULE_WINESERVER:?}
wine_inf=${CAPSULE_WINE_INF:?}
contained_home=${HOME:?}
/usr/bin/mkdir -p -- \
    "$contained_home" \
    "${XDG_CACHE_HOME:?}" \
    "${XDG_CONFIG_HOME:?}" \
    "${XDG_DATA_HOME:?}" \
    "${XDG_STATE_HOME:?}" || exit 125
locale_input=$1
desktop=$2
workdir=$3
entrypoint=$4
shift 4
case "$locale_input" in
    *@*)
        locale_base=${locale_input%%@*}
        locale_modifier=${locale_input#*@}
        locale_name=$locale_base.UTF-8@$locale_modifier
        ;;
    *) locale_name=$locale_input.UTF-8 ;;
esac
if [ "$locale_input" != en_US ]; then
    locale_root=$prefix/.capsule-locales
    locale_path=$locale_root/$locale_name
    locale_ready=$locale_path/.capsule-ready
    if [ ! -f "$locale_ready" ]; then
        /usr/bin/rm -rf -- "$locale_path"
        /usr/bin/mkdir -p -- "$locale_path"
        /usr/bin/localedef --no-archive -i "$locale_input" -f UTF-8 "$locale_path" || exit 127
        : > "$locale_ready"
    fi
    export LOCPATH="$locale_root"
    export LANG="$locale_name"
    export LC_ALL="$locale_name"
else
    export LANG=en_US.UTF-8
    export LC_ALL=en_US.UTF-8
fi
cd -- "$prefix/$workdir" || exit 125
# Unity Doorstop/BepInEx uses a game-local winhttp.dll proxy. Wine prefers its
# built-in implementation unless explicitly told to try the native DLL first.
# Scope the override to games that actually contain that proxy beside their
# selected executable so unrelated capsules keep Wine's normal DLL selection.
entrypoint_directory=${entrypoint%/*}
if [ -f "$prefix/$entrypoint_directory/winhttp.dll" ]; then
    WINEDLLOVERRIDES="winhttp=n,b;${WINEDLLOVERRIDES:-}"
    export WINEDLLOVERRIDES
fi
ready=$prefix/.capsule-wine-ready
system32=$prefix/drive_c/windows/system32
wine_revision=$(/usr/bin/stat -c %Y -- "$wine_inf" 2>/dev/null) || wine_revision=
prefix_revision=
if [ -f "$prefix/.update-timestamp" ]; then
    # Wine writes this text file through Win32 and therefore uses CRLF.
    prefix_revision=$(/usr/bin/tr -d '\r\n' < "$prefix/.update-timestamp") || prefix_revision=
fi
case "$prefix_revision" in
    ''|*[!0-9]*) prefix_revision= ;;
esac
if [ ! -f "$ready" ] ||
   [ -z "$wine_revision" ] ||
   [ "$prefix_revision" != "$wine_revision" ] ||
   [ ! -s "$prefix/system.reg" ] ||
   [ ! -s "$system32/shell32.dll" ] ||
   [ ! -s "$system32/user32.dll" ] ||
   [ ! -s "$system32/ucrtbase.dll" ]; then
    printf 'Capsule: Wine prefix preparation did not complete\n' >&2
    exit 126
fi
if [ -n "$desktop" ]; then
    "$wine" reg add 'HKCU\Software\Wine\Explorer' /v Desktop /t REG_SZ /d Capsule /f
    status=$?
    if [ "$status" -eq 0 ]; then
        "$wine" reg add 'HKCU\Software\Wine\Explorer\Desktops' /v Capsule /t REG_SZ /d "$desktop" /f
        status=$?
    fi
    if [ "$status" -ne 0 ]; then
        "$wineserver" -k
        "$wineserver" -w
        exit "$status"
    fi
else
    "$wine" reg delete 'HKCU\Software\Wine\Explorer' /v Desktop /f >/dev/null 2>&1 || :
fi
if [ "${CAPSULE_START_STEAM:-0}" = 1 ]; then
    steam_executable="$prefix/drive_c/Program Files (x86)/Steam/steam.exe"
    steam_login_log="$prefix/drive_c/Program Files (x86)/Steam/logs/steamui_login.txt"
    if [ ! -s "$steam_executable" ]; then
        printf 'Capsule: Steam is enabled but is not installed in this capsule\n' >&2
        exit 127
    fi
    # Record the end of the previous login log before starting Steam. Searching
    # only newly appended bytes prevents a successful older session from being
    # mistaken for readiness during the current launch.
    steam_login_offset=0
    if [ -f "$steam_login_log" ]; then
        steam_login_offset=$(/usr/bin/stat -c %s -- "$steam_login_log" 2>/dev/null) || steam_login_offset=0
    fi
    case "$steam_login_offset" in
        ''|*[!0-9]*) steam_login_offset=0 ;;
    esac
    # Keep Steam in the same prefix, network namespace, and private display as
    # the game. Its login state never comes from the host Steam installation.
    "$wine" "$steam_executable" -silent &
    steam_launcher=$!
    # Steam replaces its bootstrap process while updating, and its web helper
    # starts before account authentication and library initialization finish.
    # Wait for the current login state machine to report Success so games that
    # call SteamAPI_Init only once cannot race the client during startup.
    steam_ready=0
    steam_wait=0
    while [ "$steam_wait" -lt 60 ]; do
        if [ -f "$steam_login_log" ]; then
            steam_login_size=$(/usr/bin/stat -c %s -- "$steam_login_log" 2>/dev/null) || steam_login_size=0
            case "$steam_login_size" in
                ''|*[!0-9]*) steam_login_size=0 ;;
            esac
            # Steam may rotate or truncate its logs during a client update.
            if [ "$steam_login_size" -lt "$steam_login_offset" ]; then
                steam_login_offset=0
            fi
            if [ "$steam_login_size" -gt "$steam_login_offset" ] &&
               /usr/bin/tail -c "+$((steam_login_offset + 1))" -- "$steam_login_log" 2>/dev/null |
                 /usr/bin/grep -Fq 'SetLoginState: Success - OK'; then
                steam_ready=1
                break
            fi
        fi
        if ! /usr/bin/kill -0 "$steam_launcher" 2>/dev/null &&
           ! "$wine" tasklist /FI 'IMAGENAME eq steam.exe' 2>/dev/null |
             /usr/bin/grep -qi 'steam.exe'; then
            printf 'Capsule: the in-capsule Steam client exited during startup\n' >&2
            "$wineserver" -k >/dev/null 2>&1 || :
            "$wineserver" -w >/dev/null 2>&1 || :
            exit 127
        fi
        /usr/bin/sleep 1
        steam_wait=$((steam_wait + 1))
    done
    if [ "$steam_ready" -ne 1 ]; then
        printf 'Capsule: Steam did not finish signing in within 60 seconds; open Steam in this capsule, complete login or updates, and try again\n' >&2
        "$wineserver" -k >/dev/null 2>&1 || :
        "$wineserver" -w >/dev/null 2>&1 || :
        exit 124
    fi
fi
"$wine" "$prefix/$entrypoint" "$@"
status=$?
printf 'Capsule: Wine exited with status %s\n' "$status" >&2
if [ "${CAPSULE_WAIT_FOR_WINESERVER:-0}" = 1 ]; then
    "$wineserver" -w
else
    "$wineserver" -k
    "$wineserver" -w
fi
exit "$status""#;

const SANDBOX_NATIVE_LAUNCH_SCRIPT: &str = r#"root=${CAPSULE_ROOT:?}
contained_home=${HOME:?}
/usr/bin/mkdir -p -- \
    "$contained_home" \
    "${XDG_CACHE_HOME:?}" \
    "${XDG_CONFIG_HOME:?}" \
    "${XDG_DATA_HOME:?}" \
    "${XDG_STATE_HOME:?}" || exit 125
workdir=$1
entrypoint=$2
shift 2
cd -- "$root/$workdir" || exit 125
case "$entrypoint" in
    *.sh|*.SH) exec /bin/sh "$root/$entrypoint" "$@" ;;
    *) exec "$root/$entrypoint" "$@" ;;
esac"#;

#[derive(Clone, Debug)]
struct BundledRuntime {
    root: PathBuf,
}

impl BundledRuntime {
    fn from_environment() -> Result<Option<Self>, LaunchError> {
        let Some(root) = std::env::var_os("CAPSULE_BUNDLE_ROOT").filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        validate_host_absolute(&root)?;
        let metadata =
            std::fs::symlink_metadata(&root).map_err(|source| LaunchError::BundledRuntimeIo {
                path: root.clone(),
                source,
            })?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(LaunchError::UnsafeBundledRuntime(root));
        }
        for directory in [
            root.join("usr/bin"),
            root.join("usr/lib"),
            root.join("usr/share"),
        ] {
            let metadata = std::fs::symlink_metadata(&directory).map_err(|source| {
                LaunchError::BundledRuntimeIo {
                    path: directory.clone(),
                    source,
                }
            })?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(LaunchError::UnsafeBundledRuntime(directory));
            }
        }
        Ok(Some(Self { root }))
    }

    fn bin(&self) -> PathBuf {
        self.root.join("usr/bin")
    }

    fn lib(&self) -> PathBuf {
        self.root.join("usr/lib")
    }

    fn share(&self) -> PathBuf {
        self.root.join("usr/share")
    }

    fn search_path(&self) -> OsString {
        let mut value = self.bin().into_os_string();
        value.push(":/usr/bin:/bin");
        value
    }

    fn data_search_path(&self) -> OsString {
        let mut value = self.share().into_os_string();
        value.push(":/usr/local/share:/usr/share");
        value
    }
}

#[derive(Clone, Debug)]
struct WineTools {
    wine: PathBuf,
    wineserver: PathBuf,
    wineboot: PathBuf,
    winepath: PathBuf,
    wine_inf: PathBuf,
}

impl WineTools {
    fn from_capabilities(capabilities: &CapabilityReport) -> Result<Self, LaunchError> {
        let wine_inf = std::env::var_os("CAPSULE_WINE_INF")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/share/wine/wine.inf"));
        validate_host_absolute(&wine_inf)?;
        let metadata =
            std::fs::symlink_metadata(&wine_inf).map_err(|source| LaunchError::WineRuntimeIo {
                path: wine_inf.clone(),
                source,
            })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(LaunchError::UnsafeWineRuntime(wine_inf));
        }
        Ok(Self {
            wine: require(capabilities, Capability::Wine)?,
            wineserver: require(capabilities, Capability::WineServer)?,
            wineboot: require(capabilities, Capability::WineBoot)?,
            winepath: require(capabilities, Capability::WinePath)?,
            wine_inf,
        })
    }
}

fn add_systemd_runtime_environment(
    mut command: CommandSpec,
    bundled: Option<&BundledRuntime>,
) -> CommandSpec {
    let search_path = bundled
        .map(BundledRuntime::search_path)
        .unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    command = command.arg(systemd_value_environment("PATH", &search_path));
    let Some(bundled) = bundled else {
        return command;
    };

    for (name, value) in [
        (
            "CAPSULE_BUNDLE_ROOT",
            bundled.root.as_os_str().to_os_string(),
        ),
        ("LD_LIBRARY_PATH", bundled.lib().into_os_string()),
        ("XDG_DATA_DIRS", bundled.data_search_path()),
        (
            "GIO_EXTRA_MODULES",
            bundled.lib().join("gio/modules").into_os_string(),
        ),
        (
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            bundled.lib().join("gstreamer-1.0").into_os_string(),
        ),
        (
            "GDK_PIXBUF_MODULE_FILE",
            bundled
                .lib()
                .join("gdk-pixbuf-2.0/2.10.0/loaders.cache")
                .into_os_string(),
        ),
        (
            "MAGICK_CONFIGURE_PATH",
            bundled.share().join("capsule/imagemagick").into_os_string(),
        ),
        (
            "MAGICK_CODER_MODULE_PATH",
            bundled
                .lib()
                .join("capsule/imagemagick/coders")
                .into_os_string(),
        ),
    ] {
        command = command.arg(systemd_value_environment(name, &value));
    }
    command
}

fn add_sandwine_runtime(mut command: CommandSpec, bundled: Option<&BundledRuntime>) -> CommandSpec {
    let Some(bundled) = bundled else {
        return command;
    };
    command = command
        .arg("--pass")
        .arg(read_only_mount_argument(&bundled.root));
    for (name, value) in [
        (
            "CAPSULE_BUNDLE_ROOT",
            bundled.root.as_os_str().to_os_string(),
        ),
        ("LD_LIBRARY_PATH", bundled.lib().into_os_string()),
        ("WINEDLLPATH", bundled.lib().join("wine").into_os_string()),
        ("XDG_DATA_DIRS", bundled.data_search_path()),
    ] {
        command = command.arg("--env").arg(value_environment(name, &value));
    }
    command
}

fn add_wine_tools(mut command: CommandSpec, tools: &WineTools) -> CommandSpec {
    for (name, path) in [
        ("CAPSULE_WINE", &tools.wine),
        ("CAPSULE_WINESERVER", &tools.wineserver),
        ("CAPSULE_WINEBOOT", &tools.wineboot),
        ("CAPSULE_WINEPATH", &tools.winepath),
        ("CAPSULE_WINE_INF", &tools.wine_inf),
    ] {
        command = command.arg("--env").arg(path_environment(name, path));
    }
    command
}

fn require(
    capabilities: &CapabilityReport,
    capability: Capability,
) -> Result<PathBuf, LaunchError> {
    capabilities
        .get(capability)
        .map(Path::to_path_buf)
        .ok_or(LaunchError::MissingCapability(capability))
}

fn validate_runtime_root(path: &Path) -> Result<(), LaunchError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(LaunchError::UnsafeRuntimeRoot(path.to_path_buf()));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|source| LaunchError::RuntimeRootIo(path.to_path_buf(), source))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(LaunchError::UnsafeRuntimeRoot(path.to_path_buf()));
    }
    Ok(())
}

fn default_playback_audio_socket() -> PathBuf {
    PathBuf::from(format!(
        "/run/user/{}/pulse/capsule-playback-native",
        getuid().as_raw()
    ))
}

fn validate_playback_audio_socket(path: &Path) -> Result<(), LaunchError> {
    validate_host_absolute(path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|source| {
        LaunchError::PlaybackAudioBrokerUnavailable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if !metadata.file_type().is_socket() {
        return Err(LaunchError::PlaybackAudioBrokerNotSocket(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn read_write_mount_argument(root: &Path) -> OsString {
    let mut argument = root.as_os_str().to_os_string();
    argument.push(":rw");
    argument
}

fn read_only_mount_argument(path: &Path) -> OsString {
    let mut argument = path.as_os_str().to_os_string();
    argument.push(":ro");
    argument
}

fn compatibility_font_dir() -> Result<PathBuf, LaunchError> {
    if let Some(path) = std::env::var_os("CAPSULE_COMPAT_FONT_DIR").filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        validate_compatibility_font_dir(&path)?;
        return Ok(path);
    }

    let candidates = [
        PathBuf::from(INSTALLED_COMPATIBILITY_FONT_DIR),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/windows-compat"),
    ];
    for candidate in &candidates {
        match std::fs::symlink_metadata(candidate) {
            Ok(_) => {
                validate_compatibility_font_dir(candidate)?;
                return Ok(candidate.clone());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LaunchError::CompatibilityFontPackIo {
                    path: candidate.clone(),
                    source,
                });
            }
        }
    }

    Err(LaunchError::CompatibilityFontPackUnavailable(
        candidates.into_iter().collect(),
    ))
}

fn validate_compatibility_font_dir(path: &Path) -> Result<(), LaunchError> {
    validate_host_absolute(path)?;
    if path.to_str().is_none_or(|path| path.contains(['\n', '\r'])) {
        return Err(LaunchError::UnsafeCompatibilityFontPack(path.to_path_buf()));
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|source| LaunchError::CompatibilityFontPackIo {
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(LaunchError::UnsafeCompatibilityFontPack(path.to_path_buf()));
    }
    for file in COMPATIBILITY_FONT_FILES {
        let asset = path.join(file);
        let metadata = std::fs::symlink_metadata(&asset).map_err(|source| {
            LaunchError::CompatibilityFontPackIo {
                path: asset.clone(),
                source,
            }
        })?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(LaunchError::UnsafeCompatibilityFontPack(asset));
        }
    }
    Ok(())
}

fn ensure_font_pack_outside_root(fonts: &Path, root: &Path) -> Result<(), LaunchError> {
    if fonts.starts_with(root) {
        Err(LaunchError::CompatibilityFontPackInsideCapsule(
            fonts.to_path_buf(),
        ))
    } else {
        Ok(())
    }
}

fn dxvk_dir() -> Result<PathBuf, LaunchError> {
    if let Some(path) = std::env::var_os("CAPSULE_DXVK_DIR").filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        validate_dxvk_dir(&path)?;
        return Ok(path);
    }

    let candidates = [
        PathBuf::from(INSTALLED_DXVK_DIR),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dxvk/windows-compat"),
    ];
    for candidate in &candidates {
        match std::fs::symlink_metadata(candidate) {
            Ok(_) => {
                validate_dxvk_dir(candidate)?;
                return Ok(candidate.clone());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LaunchError::DxvkPackIo {
                    path: candidate.clone(),
                    source,
                });
            }
        }
    }

    Err(LaunchError::DxvkPackUnavailable(
        candidates.into_iter().collect(),
    ))
}

fn validate_dxvk_dir(path: &Path) -> Result<(), LaunchError> {
    validate_host_absolute(path)?;
    if path.to_str().is_none_or(|path| path.contains(['\n', '\r'])) {
        return Err(LaunchError::UnsafeDxvkPack(path.to_path_buf()));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|source| LaunchError::DxvkPackIo {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(LaunchError::UnsafeDxvkPack(path.to_path_buf()));
    }
    for architecture in ["x32", "x64"] {
        let architecture_dir = path.join(architecture);
        let metadata = std::fs::symlink_metadata(&architecture_dir).map_err(|source| {
            LaunchError::DxvkPackIo {
                path: architecture_dir.clone(),
                source,
            }
        })?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(LaunchError::UnsafeDxvkPack(architecture_dir));
        }
        for file in DXVK_DLL_FILES {
            let asset = path.join(architecture).join(file);
            let metadata =
                std::fs::symlink_metadata(&asset).map_err(|source| LaunchError::DxvkPackIo {
                    path: asset.clone(),
                    source,
                })?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() == 0
            {
                return Err(LaunchError::UnsafeDxvkPack(asset));
            }
        }
    }
    let license = path.join("LICENSE");
    let metadata =
        std::fs::symlink_metadata(&license).map_err(|source| LaunchError::DxvkPackIo {
            path: license.clone(),
            source,
        })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
        return Err(LaunchError::UnsafeDxvkPack(license));
    }
    Ok(())
}

fn ensure_dxvk_outside_root(dxvk: &Path, root: &Path) -> Result<(), LaunchError> {
    if dxvk.starts_with(root) {
        Err(LaunchError::DxvkPackInsideCapsule(dxvk.to_path_buf()))
    } else {
        Ok(())
    }
}

fn path_environment(name: &str, path: &Path) -> OsString {
    let mut assignment = OsString::from(format!("{name}="));
    assignment.push(path.as_os_str());
    assignment
}

fn systemd_path_environment(name: &str, path: &Path) -> OsString {
    let mut assignment = OsString::from(format!("--setenv={name}="));
    assignment.push(path.as_os_str());
    assignment
}

fn value_environment(name: &str, value: &std::ffi::OsStr) -> OsString {
    let mut assignment = OsString::from(format!("{name}="));
    assignment.push(value);
    assignment
}

fn systemd_value_environment(name: &str, value: &std::ffi::OsStr) -> OsString {
    let mut assignment = OsString::from(format!("--setenv={name}="));
    assignment.push(value);
    assignment
}

fn wine_debug_environment() -> OsString {
    let mut assignment = OsString::from("WINEDEBUG=");
    assignment.push(
        std::env::var_os("CAPSULE_WINEDEBUG")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("err+all")),
    );
    assignment
}

fn ensure_inside_drive_c(path: &Path) -> Result<(), LaunchError> {
    let first = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .ok_or_else(|| LaunchError::NotInsideDriveC(path.to_path_buf()))?;
    if first == "drive_c" {
        Ok(())
    } else {
        Err(LaunchError::NotInsideDriveC(path.to_path_buf()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("capsule record is invalid: {0}")]
    InvalidRecord(crate::model::ModelError),
    #[error(transparent)]
    InvalidPath(#[from] PathValidationError),
    #[error(transparent)]
    InvalidCapabilityOverride(#[from] CapabilityError),
    #[error("required sandbox component is missing: {0:?}")]
    MissingCapability(Capability),
    #[error(
        "LAN-only and custom endpoint filtering are not implemented; network access remains blocked"
    )]
    FilteredNetworkUnavailable,
    #[error("selected-controller brokering is not implemented; raw /dev/input will not be exposed")]
    ControllerBrokerUnavailable,
    #[error("playback-only audio broker is unavailable at {path:?}: {source}")]
    PlaybackAudioBrokerUnavailable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("playback-only audio broker is not a Unix socket: {0:?}")]
    PlaybackAudioBrokerNotSocket(PathBuf),
    #[error("the current private-display backend cannot honestly disable direct GPU access")]
    SoftwareDisplayUnavailable,
    #[error("single-file images require the mount-and-supervise launch path")]
    ImageSupervisorRequired,
    #[error("capsule runtime root is not a safe absolute directory: {0:?}")]
    UnsafeRuntimeRoot(PathBuf),
    #[error("cannot inspect capsule runtime root {0:?}: {1}")]
    RuntimeRootIo(PathBuf, #[source] io::Error),
    #[error("bundled runtime root is not a safe absolute directory: {0:?}")]
    UnsafeBundledRuntime(PathBuf),
    #[error("cannot inspect bundled runtime path {path:?}: {source}")]
    BundledRuntimeIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Wine runtime file is not a safe regular file: {0:?}")]
    UnsafeWineRuntime(PathBuf),
    #[error("cannot inspect Wine runtime file {path:?}: {source}")]
    WineRuntimeIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Wine entrypoint must live below drive_c: {0:?}")]
    NotInsideDriveC(PathBuf),
    #[error("trusted launch status must stay outside the game-visible prefix: {0:?}")]
    UnsafeStatusPath(PathBuf),
    #[error("Capsule compatibility fonts are unavailable; checked {0:?}")]
    CompatibilityFontPackUnavailable(Vec<PathBuf>),
    #[error("Capsule compatibility font pack is not a safe directory of regular files: {0:?}")]
    UnsafeCompatibilityFontPack(PathBuf),
    #[error("cannot inspect Capsule compatibility font asset {path:?}: {source}")]
    CompatibilityFontPackIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Capsule compatibility fonts must stay outside the writable capsule root: {0:?}")]
    CompatibilityFontPackInsideCapsule(PathBuf),
    #[error("Capsule DXVK runtime is unavailable; checked {0:?}")]
    DxvkPackUnavailable(Vec<PathBuf>),
    #[error("Capsule DXVK runtime is not a safe directory of regular files: {0:?}")]
    UnsafeDxvkPack(PathBuf),
    #[error("cannot inspect Capsule DXVK runtime asset {path:?}: {source}")]
    DxvkPackIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Capsule DXVK runtime must stay outside the writable capsule root: {0:?}")]
    DxvkPackInsideCapsule(PathBuf),
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use super::*;
    use crate::model::{Permissions, StorageKind};

    fn executable(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        File::create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn socket_tempdir() -> tempfile::TempDir {
        if let Some(runtime_root) = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path.is_dir())
            && let Ok(directory) = tempfile::tempdir_in(runtime_root)
        {
            return directory;
        }
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn offline_wine_plan_preserves_arguments_and_gates_audio() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir(&prefix).unwrap();
        let tools = temp.path().join("tools");
        fs::create_dir(&tools).unwrap();
        for name in [
            "bwrap",
            "gamescope",
            "Xwayland",
            "capsule-xwayland",
            "capsule-window-center",
            "libcapsule.so",
            "wl-paste",
            "wine",
            "wineserver",
            "wineboot",
            "winepath",
            "systemd-run",
            "sandwine",
        ] {
            executable(&tools, name);
        }
        let mut capabilities = CapabilityReport::default();
        for capability in [
            Capability::Bubblewrap,
            Capability::Gamescope,
            Capability::Xwayland,
            Capability::XwaylandWrapper,
            Capability::WindowCenter,
            Capability::ClipboardGuard,
            Capability::WlPaste,
            Capability::Wine,
            Capability::WineServer,
            Capability::WineBoot,
            Capability::WinePath,
            Capability::SystemdRun,
            Capability::Sandwine,
        ] {
            capabilities
                .insert_override(capability, tools.join(capability.executable_name()))
                .unwrap();
        }

        let mut record = CapsuleRecord::new(
            "Odd; game",
            StorageKind::DirectoryDev {
                path: prefix.clone(),
            },
            "drive_c/Odd Game/game.exe",
            RunnerKind::Wine,
        );
        record.permissions = Permissions::offline_game();
        record.permissions.controllers = false;
        record.working_dir = Some(PathBuf::from("drive_c/Working Directory"));
        record.arguments = vec!["$(touch /tmp/nope)".into(), "two words".into()];

        let plan = build_launch_plan_with(&record, &capabilities).unwrap();
        let compatibility_fonts = compatibility_font_dir().unwrap();
        let compatibility_font_mount = read_only_mount_argument(&compatibility_fonts);
        let dxvk = dxvk_dir().unwrap();
        let dxvk_mount = read_only_mount_argument(&dxvk);
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "two words")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "$(touch /tmp/nope)")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "SHELL=/bin/sh")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == WINE_DXVK_OVERRIDES)
        );
        assert!(plan.command.args.iter().any(|argument| argument == "sh"));
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--host-x11-danger-danger")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--no-wine")
        );
        let mut expected_prefix_mount = prefix.as_os_str().to_os_string();
        expected_prefix_mount.push(":rw");
        assert!(plan.command.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == expected_prefix_mount
        }));
        assert!(plan.command.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == compatibility_font_mount
        }));
        assert!(
            plan.command
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--pass" && arguments[1] == dxvk_mount })
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--dotwine")
        );
        for (name, path) in [
            ("HOME", prefix.join(".capsule-home")),
            ("WINEPREFIX", prefix.clone()),
            ("XDG_CACHE_HOME", prefix.join(".capsule-home/cache")),
            ("XDG_CONFIG_HOME", prefix.join(".capsule-home/config")),
            ("XDG_DATA_HOME", prefix.join(".capsule-home/data")),
            ("XDG_STATE_HOME", prefix.join(".capsule-home/state")),
        ] {
            let expected = path_environment(name, &path);
            assert!(
                plan.command
                    .args
                    .windows(2)
                    .any(|arguments| { arguments[0] == "--env" && arguments[1] == expected })
            );
        }
        assert!(
            plan.command
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--env" && arguments[1] == "USER=capsule" })
        );
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("prefix=${WINEPREFIX:?}"));
        assert!(!SANDBOX_WINE_LAUNCH_SCRIPT.contains("$HOME/.wine"));
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--wayland" || argument == "--expose-wayland")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--setenv=DISPLAY=:99999")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--expand-environment=no")
        );
        assert!(
            plan.command
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--xwayland-count" && arguments[1] == "1" })
        );
        assert!(
            plan.command
                .args
                .windows(2)
                .any(|arguments| arguments[0] == "--backend" && arguments[1] == "wayland")
        );
        assert!(plan.command.args.windows(4).any(|arguments| {
            arguments[0] == "--nested-width"
                && arguments[1] == "1920"
                && arguments[2] == "--nested-height"
                && arguments[3] == "1080"
        }));
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--borderless")
        );
        assert!(plan.command.args.windows(10).any(|arguments| {
            arguments[0] == "/bin/sh"
                && arguments[1] == "-c"
                && arguments[2] == PRIVATE_DISPLAY_LAUNCH_SCRIPT
                && arguments[3] == "capsule-private-display"
                && arguments[4].is_empty()
                && arguments[5] == "0"
                && arguments[6] == "0"
                && arguments[7] == "0"
                && arguments[8].is_empty()
                && arguments[9] == tools.join("sandwine")
        }));
        assert!(plan.command.args.windows(4).any(|arguments| {
            let expected_preload = OsString::from(format!(
                "LD_PRELOAD={}",
                tools.join("libcapsule.so").display()
            ));
            arguments[0] == "/usr/bin/env"
                && arguments[1] == expected_preload
                && arguments[2] == "CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD=1"
                && arguments[3] == tools.join("gamescope")
        }));
        assert!(
            PRIVATE_DISPLAY_LAUNCH_SCRIPT
                .contains("unset LD_PRELOAD CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD")
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == TRUSTED_STATUS_LAUNCH_SCRIPT)
        );
        assert!(
            plan.command
                .args
                .windows(2)
                .any(|arguments| arguments[0] == "--scaler" && arguments[1] == "fit")
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--force-windows-fullscreen")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--property=LimitCORE=0")
        );
        let launch_script = plan
            .command
            .args
            .iter()
            .find(|argument| argument == &&OsString::from(SANDBOX_WINE_LAUNCH_SCRIPT))
            .expect("fixed launch script");
        assert!(!launch_script.to_string_lossy().contains("touch /tmp/nope"));

        let preparation = build_wine_prepare_plan_with(&record, &capabilities)
            .unwrap()
            .expect("Wine preparation command");
        assert!(
            preparation
                .args
                .iter()
                .any(|argument| argument == "--setenv=DISPLAY=")
        );
        assert!(
            preparation
                .args
                .iter()
                .any(|argument| argument == "--setenv=WAYLAND_DISPLAY=")
        );
        assert!(
            preparation
                .args
                .iter()
                .any(|argument| { argument == &OsString::from(SANDBOX_WINE_PREPARE_SCRIPT) })
        );
        assert!(preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == compatibility_font_mount
        }));
        assert!(preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--env"
                && arguments[1] == path_environment("CAPSULE_COMPAT_FONTS", &compatibility_fonts)
        }));
        assert!(preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == read_only_mount_argument(&dxvk)
        }));
        assert!(preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--env" && arguments[1] == path_environment("CAPSULE_DXVK_DIR", &dxvk)
        }));
        assert!(preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--env"
                && arguments[1] == OsString::from(format!("CAPSULE_DXVK_ID={DXVK_PACK_ID}"))
        }));
        assert!(preparation.args.windows(2).any(|arguments| {
            let expected = OsString::from(format!(
                "CAPSULE_COMPAT_FONTS_ID={COMPATIBILITY_FONT_PACK_ID}"
            ));
            arguments[0] == "--env" && arguments[1] == expected
        }));
        assert!(
            !preparation
                .args
                .iter()
                .any(|argument| argument == "--host-x11-danger-danger"
                    || argument == "--wayland"
                    || argument == "--pulseaudio")
        );
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("unset DISPLAY WAYLAND_DISPLAY"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("\"$wineboot\" --init"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("\"$wineserver\" -w"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains(".update-timestamp"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("\"$wine_inf\""));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("/usr/bin/tr -d '\\r\\n'"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains(".capsule-compat-fonts"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("\"$winepath\" -w \"$font_dir\""));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("'MS Gothic' 'MS PGothic' 'MS UI Gothic'"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains(".capsule-dxvk"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("install_dxvk_arch"));
        assert!(SANDBOX_WINE_PREPARE_SCRIPT.contains("'SimSun' 'NSimSun'"));
        assert!(
            SANDBOX_WINE_PREPARE_SCRIPT
                .contains("HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Fonts")
        );
        assert!(!SANDBOX_WINE_LAUNCH_SCRIPT.contains("CAPSULE_WINEBOOT"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains(".update-timestamp"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("/usr/bin/tr -d '\\r\\n'"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("entrypoint_directory=${entrypoint%/*}"));
        assert!(
            SANDBOX_WINE_LAUNCH_SCRIPT
                .contains("WINEDLLOVERRIDES=\"winhttp=n,b;${WINEDLLOVERRIDES:-}\"")
        );
        assert!(plan.command.args.windows(6).any(|arguments| {
            arguments[0] == "capsule-wine-launch"
                && arguments[1] == "en_US"
                && arguments[2].is_empty()
                && arguments[3] == "drive_c/Working Directory"
                && arguments[4] == "drive_c/Odd Game/game.exe"
                && arguments[5] == "$(touch /tmp/nope)"
        }));

        let mut steam_record = record.clone();
        steam_record.wine_steam = true;
        let steam_plan = build_launch_plan_with(&steam_record, &capabilities).unwrap();
        assert!(steam_plan.command.args.windows(2).any(|arguments| {
            arguments[0] == "--env" && arguments[1] == "CAPSULE_START_STEAM=1"
        }));
        assert!(
            SANDBOX_WINE_LAUNCH_SCRIPT
                .contains("$prefix/drive_c/Program Files (x86)/Steam/steam.exe")
        );
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("steamui_login.txt"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("SetLoginState: Success - OK"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("steam_login_offset=$(/usr/bin/stat -c %s"));
        assert!(!SANDBOX_WINE_LAUNCH_SCRIPT.contains("steamwebhelper.exe"));

        let utility_plan =
            build_wine_utility_launch_plan_with_status(&record, &capabilities, None).unwrap();
        assert!(utility_plan.command.args.windows(2).any(|arguments| {
            arguments[0] == "--env" && arguments[1] == "CAPSULE_WAIT_FOR_WINESERVER=1"
        }));
        assert!(
            utility_plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--setenv=XWAYLAND_NO_GLAMOR=1")
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--setenv=XWAYLAND_NO_GLAMOR=1")
        );
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("${CAPSULE_WAIT_FOR_WINESERVER:-0}"));

        record.wine_virtual_desktop = Some(crate::model::WineVirtualDesktop {
            width: 640,
            height: 480,
        });
        let desktop_plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert!(desktop_plan.command.args.windows(4).any(|arguments| {
            arguments[0] == "--nested-width"
                && arguments[1] == "640"
                && arguments[2] == "--nested-height"
                && arguments[3] == "480"
        }));
        assert!(desktop_plan.command.args.windows(10).any(|arguments| {
            arguments[0] == "/bin/sh"
                && arguments[1] == "-c"
                && arguments[2] == PRIVATE_DISPLAY_LAUNCH_SCRIPT
                && arguments[3] == "capsule-private-display"
                && arguments[4] == tools.join("capsule-window-center")
                && arguments[5] == "640"
                && arguments[6] == "480"
                && arguments[7] == "0"
                && arguments[8].is_empty()
                && arguments[9] == tools.join("sandwine")
        }));
        assert!(PRIVATE_DISPLAY_LAUNCH_SCRIPT.contains("\"$$\""));
        assert!(desktop_plan.command.args.windows(6).any(|arguments| {
            arguments[0] == "capsule-wine-launch"
                && arguments[1] == "en_US"
                && arguments[2] == "640x480"
                && arguments[3] == "drive_c/Working Directory"
                && arguments[4] == "drive_c/Odd Game/game.exe"
                && arguments[5] == "$(touch /tmp/nope)"
        }));

        let mut clipboard_record = record.clone();
        clipboard_record.permissions.clipboard = true;
        let clipboard_plan = build_launch_plan_with(&clipboard_record, &capabilities).unwrap();
        assert!(
            !clipboard_plan
                .command
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().starts_with("LD_PRELOAD="))
        );
        assert!(
            !clipboard_plan
                .command
                .args
                .iter()
                .any(|argument| argument == "CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD=1")
        );
        assert!(clipboard_plan.command.args.windows(10).any(|arguments| {
            arguments[0] == "/bin/sh"
                && arguments[1] == "-c"
                && arguments[2] == PRIVATE_DISPLAY_LAUNCH_SCRIPT
                && arguments[3] == "capsule-private-display"
                && arguments[4] == tools.join("capsule-window-center")
                && arguments[5] == "640"
                && arguments[6] == "480"
                && arguments[7] == "1"
                && arguments[8] == tools.join("wl-paste")
                && arguments[9] == tools.join("sandwine")
        }));

        record.wine_locale = WineLocale::japanese();
        let japanese_plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert!(japanese_plan.command.args.windows(6).any(|arguments| {
            arguments[0] == "capsule-wine-launch"
                && arguments[1] == "ja_JP"
                && arguments[2] == "640x480"
                && arguments[3] == "drive_c/Working Directory"
                && arguments[4] == "drive_c/Odd Game/game.exe"
                && arguments[5] == "$(touch /tmp/nope)"
        }));
        record.wine_locale = WineLocale::russian();
        let russian_plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert!(russian_plan.command.args.windows(6).any(|arguments| {
            arguments[0] == "capsule-wine-launch"
                && arguments[1] == "ru_RU"
                && arguments[2] == "640x480"
                && arguments[3] == "drive_c/Working Directory"
                && arguments[4] == "drive_c/Odd Game/game.exe"
                && arguments[5] == "$(touch /tmp/nope)"
        }));

        let status_path = temp.path().join("contained-exit-status");
        let status_plan =
            build_launch_plan_with_status(&record, &capabilities, Some(&status_path)).unwrap();
        assert!(status_plan.command.args.windows(5).any(|arguments| {
            arguments[0] == "/bin/sh"
                && arguments[1] == "-c"
                && arguments[2] == TRUSTED_STATUS_LAUNCH_SCRIPT
                && arguments[3] == "capsule-status"
                && arguments[4] == status_path
        }));
        assert!(
            SANDBOX_WINE_LAUNCH_SCRIPT
                .contains("\"$wine\" reg add 'HKCU\\Software\\Wine\\Explorer'")
        );
        assert!(
            SANDBOX_WINE_LAUNCH_SCRIPT
                .contains("\"$wine\" reg add 'HKCU\\Software\\Wine\\Explorer\\Desktops'")
        );
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains(".capsule-wine-ready"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("$system32/shell32.dll"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("/usr/bin/rm -rf --"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("/usr/bin/localedef --no-archive"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("locale_input=$1"));
        assert!(SANDBOX_WINE_LAUNCH_SCRIPT.contains("locale_modifier=${locale_input#*@}"));
        assert!(!SANDBOX_WINE_LAUNCH_SCRIPT.contains("\"$prefix/drive_c/Game\""));

        assert_eq!(gamescope_nested_size(&record), (640, 480));

        let mut missing_center = CapabilityReport::default();
        for capability in [
            Capability::Bubblewrap,
            Capability::Gamescope,
            Capability::Xwayland,
            Capability::XwaylandWrapper,
            Capability::ClipboardGuard,
            Capability::WlPaste,
            Capability::Wine,
            Capability::WineServer,
            Capability::WineBoot,
            Capability::WinePath,
            Capability::SystemdRun,
            Capability::Sandwine,
        ] {
            missing_center
                .insert_override(capability, tools.join(capability.executable_name()))
                .unwrap();
        }
        assert!(matches!(
            build_launch_plan_with(&record, &missing_center),
            Err(LaunchError::MissingCapability(Capability::WindowCenter))
        ));

        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--pulseaudio")
        );

        record.permissions.audio = AudioPolicy::PlaybackAndMicrophone;
        let audible_plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert_eq!(
            audible_plan
                .command
                .args
                .iter()
                .filter(|argument| *argument == "--pulseaudio")
                .count(),
            1
        );
        assert!(
            audible_plan
                .warnings
                .iter()
                .any(|warning| warning.contains("microphone access is enabled"))
        );

        // Real playback broker sockets live in XDG_RUNTIME_DIR. Prefer the
        // same filesystem here because some hardened test runners disallow
        // creating named Unix sockets in their generic temporary mount.
        let socket_temp = socket_tempdir();
        let playback_socket = socket_temp.path().join("capsule-playback-native");
        let _playback_listener = match UnixListener::bind(&playback_socket) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping playback-socket assertions: test sandbox denied Unix socket creation"
                );
                return;
            }
            Err(error) => panic!("could not create playback test socket: {error}"),
        };
        record.permissions.audio = AudioPolicy::PlaybackOnly;
        let playback_plan = build_launch_plan_with_runtime(
            &record,
            &capabilities,
            None,
            Some(&playback_socket),
            false,
        )
        .unwrap();
        let mut expected_mount = playback_socket.as_os_str().to_os_string();
        expected_mount.push(":ro");
        let mut expected_server = OsString::from("PULSE_SERVER=unix:");
        expected_server.push(playback_socket.as_os_str());
        assert!(
            playback_plan
                .command
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--pass" && arguments[1] == expected_mount })
        );
        assert!(
            playback_plan
                .command
                .args
                .windows(2)
                .any(|arguments| { arguments[0] == "--env" && arguments[1] == expected_server })
        );
        assert!(
            !playback_plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--pulseaudio")
        );
        assert!(playback_plan.warnings.is_empty());

        record.wine_graphics_backend = WineGraphicsBackend::WineD3d;
        let compatibility_plan = build_launch_plan_with_runtime(
            &record,
            &capabilities,
            None,
            Some(&playback_socket),
            false,
        )
        .unwrap();
        assert!(
            compatibility_plan
                .command
                .args
                .iter()
                .any(|argument| argument == WINE_D3D_OVERRIDES)
        );
        assert!(compatibility_plan.command.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == read_only_mount_argument(&dxvk)
        }));
        let compatibility_preparation = build_wine_prepare_plan_with(&record, &capabilities)
            .unwrap()
            .expect("WineD3D preparation command");
        assert!(compatibility_preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--pass" && arguments[1] == read_only_mount_argument(&dxvk)
        }));
        assert!(compatibility_preparation.args.windows(2).any(|arguments| {
            arguments[0] == "--env" && arguments[1] == path_environment("CAPSULE_DXVK_DIR", &dxvk)
        }));
    }

    #[test]
    #[ignore = "requires a user systemd manager and unprivileged Bubblewrap"]
    fn fresh_wine_prefix_is_prepared_without_a_display() {
        let temp = tempfile::tempdir().unwrap();
        let prefix = temp.path().join("prefix");
        fs::create_dir_all(prefix.join("drive_c/Game")).unwrap();
        let mut record = CapsuleRecord::new(
            "Headless Wine preparation",
            StorageKind::DirectoryDev {
                path: prefix.clone(),
            },
            "drive_c/Game/game.exe",
            RunnerKind::Wine,
        );
        record.permissions = Permissions::offline_game();

        let capabilities = detect_with_environment_override().unwrap();
        let command = build_wine_prepare_plan_with(&record, &capabilities)
            .unwrap()
            .expect("Wine preparation command");
        let status = command.execute().unwrap();
        assert!(status.success(), "preparation returned {status}");
        assert!(prefix.join(".capsule-wine-ready").is_file());
        let dxvk = dxvk_dir().unwrap();
        assert_eq!(
            fs::read_link(prefix.join("drive_c/windows/system32/d3d11.dll")).unwrap(),
            dxvk.join("x64/d3d11.dll")
        );
        assert_eq!(
            fs::read_link(prefix.join("drive_c/windows/syswow64/d3d11.dll")).unwrap(),
            dxvk.join("x32/d3d11.dll")
        );
        assert_eq!(
            fs::read_to_string(prefix.join(".capsule-dxvk")).unwrap(),
            format!("{DXVK_PACK_ID}\n{}\nwin64\n", dxvk.display())
        );
        assert_eq!(
            fs::read_to_string(prefix.join(".capsule-compat-fonts")).unwrap(),
            format!(
                "{COMPATIBILITY_FONT_PACK_ID}\n{}\n",
                compatibility_font_dir().unwrap().display()
            )
        );
        let system_registry = fs::read_to_string(prefix.join("system.reg")).unwrap();
        assert!(system_registry.contains("\"MS Gothic (TrueType)\"="));
        assert!(system_registry.contains("msgothic.ttc"));
        assert!(system_registry.contains("\"SimSun (TrueType)\"="));
        assert!(system_registry.contains("simsun.ttc"));
        assert!(
            prefix
                .join("drive_c/windows/system32/user32.dll")
                .metadata()
                .unwrap()
                .len()
                > 0
        );
        assert_eq!(
            fs::read_to_string(prefix.join(".update-timestamp"))
                .unwrap()
                .trim(),
            fs::metadata("/usr/share/wine/wine.inf")
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string()
        );

        // The marker turns later preparation passes into validation-only
        // work: do not start Wine again or rewrite a stable prefix on every
        // launch.
        let marker = fs::read_to_string(prefix.join(".capsule-compat-fonts")).unwrap();
        let second_status = command.execute().unwrap();
        assert!(
            second_status.success(),
            "second preparation returned {second_status}"
        );
        assert_eq!(
            fs::read_to_string(prefix.join(".capsule-compat-fonts")).unwrap(),
            marker
        );
        assert_eq!(
            fs::read_to_string(prefix.join("system.reg")).unwrap(),
            system_registry
        );
    }

    #[test]
    fn internet_only_network_uses_private_filtered_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("tools");
        fs::create_dir(&tools).unwrap();
        let mut capabilities = CapabilityReport::default();
        for capability in Capability::ALL {
            let executable = executable(&tools, capability.executable_name());
            capabilities
                .insert_override(capability, executable)
                .unwrap();
        }
        let mut record = CapsuleRecord::new(
            "Online",
            StorageKind::DirectoryDev {
                path: temp.path().to_path_buf(),
            },
            "drive_c/game.exe",
            RunnerKind::Wine,
        );
        record.permissions = Permissions::online_game();
        record.permissions.controllers = false;
        let plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert!(plan.command.args.windows(7).any(|arguments| {
            arguments[0] == tools.join("capsule-network")
                && arguments[1] == "--slirp4netns"
                && arguments[2] == tools.join("slirp4netns")
                && arguments[3] == "--nft"
                && arguments[4] == tools.join("nft")
                && arguments[5] == "--"
                && arguments[6] == tools.join("sandwine")
        }));
        assert_eq!(
            plan.command
                .args
                .iter()
                .filter(|argument| *argument == "--network")
                .count(),
            1
        );

        let mut missing_helper = CapabilityReport::default();
        for capability in Capability::ALL {
            if capability != Capability::NetworkHelper {
                missing_helper
                    .insert_override(capability, tools.join(capability.executable_name()))
                    .unwrap();
            }
        }
        assert!(matches!(
            build_launch_plan_with(&record, &missing_helper),
            Err(LaunchError::MissingCapability(Capability::NetworkHelper))
        ));

        record.permissions.network = NetworkPolicy::Lan;
        let lan_plan = build_launch_plan_with(&record, &capabilities).unwrap();
        assert!(
            lan_plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--network")
        );
        assert!(
            !lan_plan
                .command
                .args
                .iter()
                .any(|argument| argument == &tools.join("capsule-network"))
        );
        assert!(
            lan_plan
                .warnings
                .iter()
                .any(|warning| warning.contains("shares the host network namespace"))
        );

        record.permissions.network = NetworkPolicy::LanOnly;
        assert!(matches!(
            build_launch_plan_with(&record, &capabilities),
            Err(LaunchError::FilteredNetworkUnavailable)
        ));
    }

    #[test]
    fn bundled_compatibility_font_pack_is_complete_and_licensed() {
        let fonts = compatibility_font_dir().unwrap();
        for file in COMPATIBILITY_FONT_FILES {
            assert!(
                fonts.join(file).is_file(),
                "missing compatibility font {file}"
            );
        }
        assert!(fonts.join("NOTICE.md").is_file());
        assert!(fonts.join("LICENSE.OFL.txt").is_file());
    }

    #[test]
    fn bundled_dxvk_pack_is_complete_and_licensed() {
        let dxvk = dxvk_dir().unwrap();
        validate_dxvk_dir(&dxvk).unwrap();
        for architecture in ["x32", "x64"] {
            for file in DXVK_DLL_FILES {
                assert!(
                    dxvk.join(architecture).join(file).is_file(),
                    "missing DXVK asset {architecture}/{file}"
                );
            }
        }
        assert!(dxvk.join("LICENSE").is_file());
        assert!(dxvk.join("NOTICE.md").is_file());
    }

    #[test]
    fn native_launcher_stays_in_the_same_sandbox_and_does_not_require_wine() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("prefix");
        fs::create_dir(&root).unwrap();
        let tools = temp.path().join("tools");
        fs::create_dir(&tools).unwrap();
        for name in [
            "bwrap",
            "gamescope",
            "Xwayland",
            "capsule-xwayland",
            "libcapsule.so",
            "systemd-run",
            "sandwine",
        ] {
            executable(&tools, name);
        }
        let mut capabilities = CapabilityReport::default();
        for capability in [
            Capability::Bubblewrap,
            Capability::Gamescope,
            Capability::Xwayland,
            Capability::XwaylandWrapper,
            Capability::ClipboardGuard,
            Capability::SystemdRun,
            Capability::Sandwine,
        ] {
            capabilities
                .insert_override(capability, tools.join(capability.executable_name()))
                .unwrap();
        }
        let mut record = CapsuleRecord::new(
            "Native game",
            StorageKind::DirectoryDev { path: root.clone() },
            "drive_c/Game/game.sh",
            RunnerKind::Native,
        );
        record.permissions = Permissions::offline_game();
        record.arguments = vec!["two words".into(), "$(touch /tmp/nope)".into()];

        let plan = build_launch_plan_with(&record, &capabilities).unwrap();
        let compatibility_fonts = compatibility_font_dir().unwrap();

        assert!(
            build_wine_prepare_plan_with(&record, &capabilities)
                .unwrap()
                .is_none()
        );

        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| argument == "--no-wine")
        );
        assert!(
            plan.command
                .args
                .iter()
                .any(|argument| { argument == &path_environment("CAPSULE_ROOT", &root) })
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| { argument.to_string_lossy().starts_with("WINEPREFIX=") })
        );
        assert!(
            !plan
                .command
                .args
                .iter()
                .any(|argument| argument == &read_only_mount_argument(&compatibility_fonts))
        );
        assert!(plan.command.args.windows(5).any(|arguments| {
            arguments[0] == "capsule-native-launch"
                && arguments[1] == "drive_c/Game"
                && arguments[2] == "drive_c/Game/game.sh"
                && arguments[3] == "two words"
                && arguments[4] == "$(touch /tmp/nope)"
        }));
        assert!(SANDBOX_NATIVE_LAUNCH_SCRIPT.contains("exec /bin/sh"));
        assert!(!SANDBOX_NATIVE_LAUNCH_SCRIPT.contains("touch /tmp/nope"));
    }
}
