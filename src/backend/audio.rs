//! Installation of Capsule's per-user playback-only PipeWire policy.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::paths;

const PIPEWIRE_PULSE_POLICY: &str = include_str!("../../assets/pipewire/60-capsule-playback.conf");
const PIPEWIRE_SINK_POLICY: &str =
    include_str!("../../assets/pipewire/60-capsule-playback-sink.conf");
const WIREPLUMBER_POLICY: &str = include_str!("../../assets/wireplumber/60-capsule-playback.conf");

const POLICY_FILES: [(&str, &str); 3] = [
    (
        "pipewire/pipewire-pulse.conf.d/60-capsule-playback.conf",
        PIPEWIRE_PULSE_POLICY,
    ),
    (
        "pipewire/pipewire.conf.d/60-capsule-playback-sink.conf",
        PIPEWIRE_SINK_POLICY,
    ),
    (
        "wireplumber/wireplumber.conf.d/60-capsule-playback.conf",
        WIREPLUMBER_POLICY,
    ),
];

const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_PROCESS_CHECK_DELAY: Duration = Duration::from_millis(150);

/// A per-launch Pulse-compatible server with one playback sink and no host
/// capture devices. Audio sent to that sink is tunneled to the user's existing
/// PulseAudio-compatible server. Keeping the broker processes owned by
/// the supervisor avoids persistent host configuration and works independently
/// of the desktop's display protocol.
pub struct PlaybackBroker {
    socket: PathBuf,
    runtime_dir: PathBuf,
    core: Child,
    tunnel: Child,
    wireplumber: Child,
    pulse: Child,
}

impl PlaybackBroker {
    pub fn start() -> Result<Self, PlaybackBrokerError> {
        let host_runtime = host_runtime_dir()?;
        let host_socket = host_runtime.join("pulse/native");
        require_socket(&host_socket, "host PulseAudio")?;

        let capsule_runtime = paths::runtime_root()?;
        create_private_runtime_root(&capsule_runtime)?;
        let runtime_identity = Uuid::new_v4().simple().to_string();
        let runtime_dir = capsule_runtime.join(format!("a-{}", &runtime_identity[..12]));
        fs::create_dir(&runtime_dir).map_err(|source| broker_io_error(&runtime_dir, source))?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
            .map_err(|source| broker_io_error(&runtime_dir, source))?;

        match start_broker_in(&runtime_dir, &host_socket) {
            Ok(broker) => Ok(broker),
            Err(error) => {
                let _ = fs::remove_dir_all(&runtime_dir);
                Err(error)
            }
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for PlaybackBroker {
    fn drop(&mut self) {
        stop_child(&mut self.pulse);
        stop_child(&mut self.wireplumber);
        stop_child(&mut self.tunnel);
        stop_child(&mut self.core);
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

fn start_broker_in(
    runtime_dir: &Path,
    host_socket: &Path,
) -> Result<PlaybackBroker, PlaybackBrokerError> {
    let pipewire = runtime_executable("CAPSULE_PIPEWIRE", "pipewire")?;
    let pipewire_pulse = runtime_executable("CAPSULE_PIPEWIRE_PULSE", "pipewire-pulse")?;
    let wireplumber = runtime_executable("CAPSULE_WIREPLUMBER", "wireplumber")?;
    let identity = Uuid::new_v4().simple().to_string();
    let core_name = format!("c-{}", &identity[..12]);
    let socket = runtime_dir.join("capsule-playback-native");
    let core_config = runtime_dir.join("core.conf");
    let tunnel_config = runtime_dir.join("tunnel.conf");
    let pulse_config = runtime_dir.join("pulse.conf");

    write_private_file(&core_config, &core_configuration(&core_name))?;
    write_private_file(
        &tunnel_config,
        &tunnel_configuration(&core_name, host_socket),
    )?;
    write_private_file(
        &pulse_config,
        &pulse_configuration(&core_name, &socket, &identity),
    )?;

    let mut core = spawn_pipewire(&pipewire, runtime_dir, &core_config, "private core")?;
    let core_socket = runtime_dir.join(&core_name);
    if let Err(error) = wait_for_socket(&core_socket, "private PipeWire core", [&mut core]) {
        stop_child(&mut core);
        return Err(error);
    }

    let mut tunnel = match spawn_pipewire(&pipewire, runtime_dir, &tunnel_config, "playback tunnel")
    {
        Ok(child) => child,
        Err(error) => {
            stop_child(&mut core);
            return Err(error);
        }
    };
    thread::sleep(BROKER_PROCESS_CHECK_DELAY);
    let tunnel_status = match tunnel.try_wait() {
        Ok(status) => status,
        Err(source) => {
            stop_child(&mut tunnel);
            stop_child(&mut core);
            return Err(PlaybackBrokerError::InspectProcess {
                role: "playback tunnel",
                source,
            });
        }
    };
    if let Some(status) = tunnel_status {
        stop_child(&mut core);
        return Err(PlaybackBrokerError::ProcessExited {
            role: "playback tunnel",
            status,
        });
    }

    let mut wireplumber = match spawn_wireplumber(&wireplumber, runtime_dir, &core_name) {
        Ok(child) => child,
        Err(error) => {
            stop_child(&mut tunnel);
            stop_child(&mut core);
            return Err(error);
        }
    };
    thread::sleep(BROKER_PROCESS_CHECK_DELAY);
    let wireplumber_status = match wireplumber.try_wait() {
        Ok(status) => status,
        Err(source) => {
            stop_child(&mut wireplumber);
            stop_child(&mut tunnel);
            stop_child(&mut core);
            return Err(PlaybackBrokerError::InspectProcess {
                role: "private session manager",
                source,
            });
        }
    };
    if let Some(status) = wireplumber_status {
        stop_child(&mut tunnel);
        stop_child(&mut core);
        return Err(PlaybackBrokerError::ProcessExited {
            role: "private session manager",
            status,
        });
    }

    let mut pulse = match spawn_pipewire(
        &pipewire_pulse,
        runtime_dir,
        &pulse_config,
        "PulseAudio frontend",
    ) {
        Ok(child) => child,
        Err(error) => {
            stop_child(&mut wireplumber);
            stop_child(&mut tunnel);
            stop_child(&mut core);
            return Err(error);
        }
    };
    if let Err(error) = wait_for_socket(&socket, "playback-only PulseAudio frontend", [&mut pulse])
    {
        stop_child(&mut pulse);
        stop_child(&mut wireplumber);
        stop_child(&mut tunnel);
        stop_child(&mut core);
        return Err(error);
    }

    Ok(PlaybackBroker {
        socket,
        runtime_dir: runtime_dir.to_path_buf(),
        core,
        tunnel,
        wireplumber,
        pulse,
    })
}

fn host_runtime_dir() -> Result<PathBuf, PlaybackBrokerError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(PlaybackBrokerError::MissingRuntimeDirectory)?;
    if runtime.is_absolute()
        && runtime.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        Ok(runtime)
    } else {
        Err(PlaybackBrokerError::UnsafeRuntimeDirectory(runtime))
    }
}

fn create_private_runtime_root(path: &Path) -> Result<(), PlaybackBrokerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(PlaybackBrokerError::UnsafeRuntimeDirectory(
                path.to_path_buf(),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| broker_io_error(path, source))?;
        }
        Err(source) => return Err(broker_io_error(path, source)),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| broker_io_error(path, source))
}

fn runtime_executable(variable: &'static str, name: &str) -> Result<PathBuf, PlaybackBrokerError> {
    let path = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CAPSULE_BUNDLE_ROOT")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|root| root.join("usr/bin").join(name))
        })
        .unwrap_or_else(|| PathBuf::from("/usr/bin").join(name));
    if !path.is_absolute() {
        return Err(PlaybackBrokerError::UnsafeExecutable(path));
    }
    let metadata = fs::metadata(&path).map_err(|source| PlaybackBrokerError::ExecutableIo {
        path: path.clone(),
        source,
    })?;
    if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
        Ok(path)
    } else {
        Err(PlaybackBrokerError::UnsafeExecutable(path))
    }
}

fn spawn_pipewire(
    executable: &Path,
    runtime_dir: &Path,
    config: &Path,
    role: &'static str,
) -> Result<Child, PlaybackBrokerError> {
    let config_name = config
        .file_name()
        .ok_or_else(|| PlaybackBrokerError::UnsafeRuntimeDirectory(config.to_path_buf()))?;
    let mut command = Command::new(executable);
    command
        .arg("-c")
        .arg(config_name)
        .env("PIPEWIRE_CONFIG_DIR", runtime_dir)
        .env("PIPEWIRE_CONFIG_NAME", config_name)
        .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(root) = std::env::var_os("CAPSULE_BUNDLE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        command
            .env("PIPEWIRE_MODULE_DIR", root.join("usr/lib/pipewire-0.3"))
            .env("SPA_PLUGIN_DIR", root.join("usr/lib/spa-0.2"));
    }
    command
        .spawn()
        .map_err(|source| PlaybackBrokerError::StartProcess {
            role,
            path: executable.to_path_buf(),
            source,
        })
}

fn spawn_wireplumber(
    executable: &Path,
    runtime_dir: &Path,
    core_name: &str,
) -> Result<Child, PlaybackBrokerError> {
    let state_dir = runtime_dir.join("wireplumber-state");
    let cache_dir = runtime_dir.join("wireplumber-cache");
    fs::create_dir(&state_dir).map_err(|source| broker_io_error(&state_dir, source))?;
    fs::create_dir(&cache_dir).map_err(|source| broker_io_error(&cache_dir, source))?;
    let mut command = Command::new(executable);
    command
        .args(["--config-file=wireplumber.conf", "--profile=policy"])
        .env("PIPEWIRE_RUNTIME_DIR", runtime_dir)
        .env("PIPEWIRE_REMOTE", core_name)
        .env("XDG_STATE_HOME", &state_dir)
        .env("XDG_CACHE_HOME", &cache_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(root) = std::env::var_os("CAPSULE_BUNDLE_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        command
            .env("PIPEWIRE_MODULE_DIR", root.join("usr/lib/pipewire-0.3"))
            .env("SPA_PLUGIN_DIR", root.join("usr/lib/spa-0.2"))
            .env("WIREPLUMBER_CONFIG_DIR", root.join("usr/share/wireplumber"))
            .env("WIREPLUMBER_DATA_DIR", root.join("usr/share/wireplumber"))
            .env(
                "WIREPLUMBER_MODULE_DIR",
                root.join("usr/lib/wireplumber-0.5"),
            );
    }
    command
        .spawn()
        .map_err(|source| PlaybackBrokerError::StartProcess {
            role: "private session manager",
            path: executable.to_path_buf(),
            source,
        })
}

fn wait_for_socket<const N: usize>(
    path: &Path,
    role: &'static str,
    mut children: [&mut Child; N],
) -> Result<(), PlaybackBrokerError> {
    let deadline = Instant::now() + BROKER_START_TIMEOUT;
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                if UnixStream::connect(path).is_ok() {
                    return Ok(());
                }
            }
            Ok(_) => return Err(PlaybackBrokerError::NotSocket(path.to_path_buf())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(broker_io_error(path, source)),
        }
        for child in &mut children {
            if let Some(status) = child
                .try_wait()
                .map_err(|source| PlaybackBrokerError::InspectProcess { role, source })?
            {
                return Err(PlaybackBrokerError::ProcessExited { role, status });
            }
        }
        if Instant::now() >= deadline {
            return Err(PlaybackBrokerError::StartTimedOut {
                role,
                path: path.to_path_buf(),
            });
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn require_socket(path: &Path, role: &'static str) -> Result<(), PlaybackBrokerError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        PlaybackBrokerError::RequiredSocketUnavailable {
            role,
            path: path.to_path_buf(),
            source,
        }
    })?;
    if metadata.file_type().is_socket() {
        Ok(())
    } else {
        Err(PlaybackBrokerError::NotSocket(path.to_path_buf()))
    }
}

fn write_private_file(path: &Path, contents: &str) -> Result<(), PlaybackBrokerError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| broker_io_error(path, source))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| broker_io_error(path, source))
}

fn stop_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn spa_string(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn core_configuration(core_name: &str) -> String {
    format!(
        r#"context.properties = {{
    core.daemon = true
    core.name = {core_name}
    link.max-buffers = 64
}}

context.spa-libs = {{
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}}

context.modules = [
    {{ name = libpipewire-module-rt args = {{ nice.level = 0 rt.prio = 0 rtportal.enabled = false rtkit.enabled = false }} flags = [ ifexists nofail ] }}
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-client-device }}
    {{ name = libpipewire-module-access args = {{ access.socket = {{ {core_name} = unrestricted {manager_name} = unrestricted }} }} }}
    {{ name = libpipewire-module-spa-node-factory }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-link-factory }}
    {{ name = libpipewire-module-metadata }}
]

context.objects = [
    {{ factory = metadata args = {{ metadata.name = default metadata.values = [ {{ key = default.audio.sink value = {{ name = capsule_playback }} }} ] }} }}
    {{ factory = spa-node-factory args = {{ factory.name = support.node.driver node.name = Dummy-Driver node.group = pipewire.dummy priority.driver = 20000 }} }}
]
"#,
        core_name = spa_string(OsStr::new(core_name)),
        manager_name = spa_string(OsStr::new(&format!("{core_name}-manager")))
    )
}

fn tunnel_configuration(core_name: &str, host_socket: &Path) -> String {
    let host_server = format!("unix:{}", host_socket.to_string_lossy());
    format!(
        r#"context.properties = {{ remote.name = {core_name} }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
    {{ name = libpipewire-module-rt args = {{ nice.level = 0 rt.prio = 0 rtportal.enabled = false rtkit.enabled = false }} flags = [ ifexists nofail ] }}
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-pulse-tunnel args = {{ tunnel.mode = sink remote.name = {core_name} pulse.server.address = {host_server} pulse.latency = 100 reconnect.interval.ms = 1000 node.name = capsule_playback node.description = "Capsule Playback" node.virtual = true }} }}
]
"#,
        core_name = spa_string(OsStr::new(core_name)),
        host_server = spa_string(OsStr::new(&host_server))
    )
}

fn pulse_configuration(core_name: &str, socket: &Path, identity: &str) -> String {
    let dbus_name = format!("org.capsule.Audio.c{identity}");
    format!(
        r#"context.properties = {{ remote.name = {core_name} link.max-buffers = 64 }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
    {{ name = libpipewire-module-rt args = {{ nice.level = 0 rt.prio = 0 rtportal.enabled = false rtkit.enabled = false }} flags = [ ifexists nofail ] }}
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-metadata }}
    {{ name = libpipewire-module-protocol-pulse args = {{ server.address = [ {socket} ] pulse.allow-module-loading = false server.dbus-name = {dbus_name} }} }}
]
pulse.properties = {{ server.address = [ {socket} ] pulse.allow-module-loading = false server.dbus-name = {dbus_name} }}
pulse.rules = [ {{ matches = [ {{ }} ] actions = {{ update-props = {{ target.object = capsule_playback channelmix.lock-volumes = true state.restore-props = false state.default-volume = 1.0 }} }} }} ]
"#,
        core_name = spa_string(OsStr::new(core_name)),
        socket = spa_string(OsStr::new(&format!("unix:{}", socket.to_string_lossy()))),
        dbus_name = spa_string(OsStr::new(&dbus_name))
    )
}

fn broker_io_error(path: &Path, source: io::Error) -> PlaybackBrokerError {
    PlaybackBrokerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackBrokerError {
    #[error("XDG_RUNTIME_DIR is required for playback audio")]
    MissingRuntimeDirectory,
    #[error("playback audio runtime directory is unsafe: {0:?}")]
    UnsafeRuntimeDirectory(PathBuf),
    #[error(transparent)]
    Paths(#[from] paths::PathError),
    #[error("{role} socket is unavailable at {path:?}: {source}")]
    RequiredSocketUnavailable {
        role: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("playback audio path is not a Unix socket: {0:?}")]
    NotSocket(PathBuf),
    #[error("playback broker executable is unsafe: {0:?}")]
    UnsafeExecutable(PathBuf),
    #[error("cannot inspect playback broker executable {path:?}: {source}")]
    ExecutableIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not start playback broker {role} at {path:?}: {source}")]
    StartProcess {
        role: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not inspect playback broker {role}: {source}")]
    InspectProcess {
        role: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("playback broker {role} exited during startup: {status}")]
    ProcessExited {
        role: &'static str,
        status: ExitStatus,
    },
    #[error("playback broker {role} did not create {path:?} within five seconds")]
    StartTimedOut { role: &'static str, path: PathBuf },
    #[error("playback broker I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub fn install_user_policy() -> Result<Vec<PathBuf>, AudioIntegrationError> {
    let config_root = user_config_root()?;
    let installed = install_policy_at(&config_root)?;
    restart_audio_services()?;
    Ok(installed)
}

fn user_config_root() -> Result<PathBuf, AudioIntegrationError> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(AudioIntegrationError::UnsafeConfigRoot(path));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(AudioIntegrationError::MissingConfigRoot)?;
    if !home.is_absolute() {
        return Err(AudioIntegrationError::UnsafeConfigRoot(home));
    }
    Ok(home.join(".config"))
}

fn install_policy_at(config_root: &Path) -> Result<Vec<PathBuf>, AudioIntegrationError> {
    if !config_root.is_absolute() {
        return Err(AudioIntegrationError::UnsafeConfigRoot(
            config_root.to_path_buf(),
        ));
    }
    // Validate all existing destinations before changing any of them. A
    // customized fragment must never leave the other two files half-updated.
    for (relative, contents) in POLICY_FILES {
        let destination = config_root.join(relative);
        if let Ok(metadata) = fs::symlink_metadata(&destination) {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(AudioIntegrationError::UnsafePolicyPath(destination));
            }
            let existing = fs::read_to_string(&destination)
                .map_err(|source| io_error(&destination, source))?;
            if existing != contents {
                return Err(AudioIntegrationError::PolicyConflict(destination));
            }
        }
    }

    let mut installed = Vec::with_capacity(POLICY_FILES.len());
    for (relative, contents) in POLICY_FILES {
        let destination = config_root.join(relative);
        let parent = destination
            .parent()
            .ok_or_else(|| AudioIntegrationError::UnsafePolicyPath(destination.clone()))?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        if destination.is_file() {
            installed.push(destination);
            continue;
        }

        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))
            .and_then(|()| temporary.write_all(contents.as_bytes()))
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| io_error(&destination, source))?;
        temporary
            .persist(&destination)
            .map_err(|error| io_error(&destination, error.error))?;
        installed.push(destination);
    }
    Ok(installed)
}

fn restart_audio_services() -> Result<(), AudioIntegrationError> {
    let systemctl = std::env::var_os("CAPSULE_SYSTEMCTL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/systemctl"));
    if !systemctl.is_absolute() {
        return Err(AudioIntegrationError::UnsafeSystemctl(systemctl));
    }
    let status = Command::new(&systemctl)
        .args([
            "--user",
            "restart",
            "pipewire.service",
            "pipewire-pulse.service",
            "wireplumber.service",
        ])
        .status()
        .map_err(|source| AudioIntegrationError::StartSystemctl {
            path: systemctl.clone(),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioIntegrationError::RestartFailed(status))
    }
}

fn io_error(path: &Path, source: io::Error) -> AudioIntegrationError {
    AudioIntegrationError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioIntegrationError {
    #[error("XDG_CONFIG_HOME or HOME is required to install audio integration")]
    MissingConfigRoot,
    #[error("audio configuration root must be absolute: {0:?}")]
    UnsafeConfigRoot(PathBuf),
    #[error("refusing to replace a non-regular audio policy path: {0:?}")]
    UnsafePolicyPath(PathBuf),
    #[error("an existing Capsule audio policy was modified; review it manually: {0:?}")]
    PolicyConflict(PathBuf),
    #[error("audio integration I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("systemctl path must be absolute: {0:?}")]
    UnsafeSystemctl(PathBuf),
    #[error("could not start {path:?}: {source}")]
    StartSystemctl {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("audio services did not restart successfully: {0}")]
    RestartFailed(std::process::ExitStatus),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_broker_exposes_only_a_playback_tunnel() {
        let identity = "0123456789abcdef0123456789abcdef";
        let pulse = pulse_configuration(
            "capsule-playback-test",
            Path::new("/run/user/1000/capsule/audio-test/pulse"),
            identity,
        );
        let tunnel = tunnel_configuration(
            "capsule-playback-test",
            Path::new("/run/user/1000/pulse/native"),
        );

        assert!(tunnel.contains("tunnel.mode = sink"));
        assert!(!tunnel.contains("tunnel.mode = source"));
        assert!(pulse.contains("pulse.allow-module-loading = false"));
        assert!(pulse.contains("target.object = capsule_playback"));
        assert!(!pulse.contains("Audio/Source"));
        assert!(!pulse.contains("host microphone"));
    }

    #[test]
    fn broker_configuration_escapes_paths_as_spa_strings() {
        let escaped = spa_string(OsStr::new("/run/user/a\\b\"c"));
        assert_eq!(escaped, r#""/run/user/a\\b\"c""#);
    }

    #[test]
    #[ignore = "requires a live Pulse-compatible user audio server"]
    fn live_ephemeral_broker_creates_a_connectable_socket() {
        let broker = PlaybackBroker::start().unwrap();
        UnixStream::connect(broker.socket()).unwrap();
        let server = format!("unix:{}", broker.socket().display());
        let sources = Command::new("/usr/bin/pactl")
            .args(["list", "short", "sources"])
            .env("PULSE_SERVER", server)
            .output()
            .unwrap();
        assert!(sources.status.success());
        let sources = String::from_utf8(sources.stdout).unwrap();
        let sources = sources.lines().collect::<Vec<_>>();
        assert_eq!(sources.len(), 1);
        assert!(sources[0].contains("capsule_playback.monitor"));

        let mut playback = Command::new("/usr/bin/paplay")
            .args([
                "--raw",
                "--rate=48000",
                "--channels=2",
                "--format=s16le",
                "/dev/zero",
            ])
            .env(
                "PULSE_SERVER",
                format!("unix:{}", broker.socket().display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(250));
        assert!(playback.try_wait().unwrap().is_none());
        let sinks = Command::new("/usr/bin/pactl")
            .args(["list", "short", "sinks"])
            .env(
                "PULSE_SERVER",
                format!("unix:{}", broker.socket().display()),
            )
            .output()
            .unwrap();
        let _ = playback.kill();
        let _ = playback.wait();
        assert!(sinks.status.success());
        let sinks = String::from_utf8(sinks.stdout).unwrap();
        assert!(sinks.contains("RUNNING"), "private sinks: {sinks}");
    }

    #[test]
    fn installs_only_the_three_owned_policy_files() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config");
        let installed = install_policy_at(&config).unwrap();
        assert_eq!(installed.len(), 3);
        for (relative, contents) in POLICY_FILES {
            assert_eq!(fs::read_to_string(config.join(relative)).unwrap(), contents);
        }
    }

    #[test]
    fn refuses_to_overwrite_a_modified_policy() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config");
        let path = config.join(POLICY_FILES[0].0);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user customization\n").unwrap();
        assert!(matches!(
            install_policy_at(&config),
            Err(AudioIntegrationError::PolicyConflict(conflict)) if conflict == path
        ));
        assert!(!config.join(POLICY_FILES[1].0).exists());
        assert!(!config.join(POLICY_FILES[2].0).exists());
    }
}
