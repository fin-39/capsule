//! Launch-scoped, playback-only access to the host PipeWire graph.

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

const FRONTEND_START_TIMEOUT: Duration = Duration::from_secs(5);

/// A private PulseAudio-protocol socket backed directly by the user's existing
/// PipeWire graph. The frontend exists only for one launch, cannot create
/// recording streams, and does not install configuration or restart services.
pub struct PlaybackBroker {
    socket: PathBuf,
    runtime_dir: PathBuf,
    pulse: Child,
}

impl PlaybackBroker {
    pub fn start() -> Result<Self, PlaybackBrokerError> {
        let host_runtime = host_runtime_dir()?;
        let remote_name = host_pipewire_remote()?;
        require_socket(&host_runtime.join(&remote_name), "host PipeWire")?;

        let capsule_runtime = paths::runtime_root()?;
        create_private_runtime_root(&capsule_runtime)?;
        let runtime_identity = Uuid::new_v4().simple().to_string();
        let runtime_dir = capsule_runtime.join(format!("a-{}", &runtime_identity[..12]));
        fs::create_dir(&runtime_dir).map_err(|source| broker_io_error(&runtime_dir, source))?;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
            .map_err(|source| broker_io_error(&runtime_dir, source))?;

        match start_frontend_in(&runtime_dir, &host_runtime, &remote_name) {
            Ok(broker) => {
                eprintln!(
                    "Capsule: using launch-scoped playback-only audio endpoint at {}",
                    broker.socket.display()
                );
                Ok(broker)
            }
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
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

fn start_frontend_in(
    runtime_dir: &Path,
    host_runtime: &Path,
    remote_name: &str,
) -> Result<PlaybackBroker, PlaybackBrokerError> {
    let pipewire_pulse = runtime_executable("CAPSULE_PIPEWIRE_PULSE", "pipewire-pulse")?;
    let identity = Uuid::new_v4().simple().to_string();
    let socket = runtime_dir.join("capsule-playback-native");
    let pulse_config = runtime_dir.join("pulse.conf");

    write_private_file(
        &pulse_config,
        &pulse_configuration(remote_name, &socket, &identity),
    )?;

    let mut pulse = spawn_pulse_frontend(
        &pipewire_pulse,
        runtime_dir,
        host_runtime,
        remote_name,
        &pulse_config,
    )?;
    if let Err(error) = wait_for_socket(&socket, "playback-only PulseAudio frontend", &mut pulse) {
        stop_child(&mut pulse);
        return Err(error);
    }

    Ok(PlaybackBroker {
        socket,
        runtime_dir: runtime_dir.to_path_buf(),
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

fn host_pipewire_remote() -> Result<String, PlaybackBrokerError> {
    let remote = std::env::var("PIPEWIRE_REMOTE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "pipewire-0".to_owned());
    let path = Path::new(&remote);
    if path.components().count() == 1 && path.file_name() == Some(OsStr::new(&remote)) {
        Ok(remote)
    } else {
        Err(PlaybackBrokerError::UnsafeRemote(remote))
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

fn spawn_pulse_frontend(
    executable: &Path,
    runtime_dir: &Path,
    host_runtime: &Path,
    remote_name: &str,
    config: &Path,
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
        .env("PIPEWIRE_RUNTIME_DIR", host_runtime)
        .env("PIPEWIRE_REMOTE", remote_name)
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
            role: "playback-only PulseAudio frontend",
            path: executable.to_path_buf(),
            source,
        })
}

fn wait_for_socket(
    path: &Path,
    role: &'static str,
    child: &mut Child,
) -> Result<(), PlaybackBrokerError> {
    let deadline = Instant::now() + FRONTEND_START_TIMEOUT;
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
        if let Some(status) = child
            .try_wait()
            .map_err(|source| PlaybackBrokerError::InspectProcess { role, source })?
        {
            return Err(PlaybackBrokerError::ProcessExited { role, status });
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

fn pulse_configuration(remote_name: &str, socket: &Path, identity: &str) -> String {
    let dbus_name = format!("org.capsule.Audio.c{identity}");
    format!(
        r#"context.properties = {{
    remote.name = {remote_name}
    link.max-buffers = 64
}}

context.spa-libs = {{
    audio.convert.* = audioconvert/libspa-audioconvert
    support.* = support/libspa-support
}}

context.modules = [
    {{ name = libpipewire-module-protocol-native }}
    {{ name = libpipewire-module-client-node }}
    {{ name = libpipewire-module-adapter }}
    {{ name = libpipewire-module-metadata }}
    {{ name = libpipewire-module-protocol-pulse }}
]

pulse.properties = {{
    server.address = [
        {{ address = {socket} client.access = restricted }}
    ]
    server.dbus-name = {dbus_name}
    pulse.allow-module-loading = false
    pulse.min.req = 1024/48000
    pulse.default.req = 2048/48000
    pulse.default.tlength = 8192/48000
    pulse.min.quantum = 1024/48000
    pulse.idle.timeout = 5
}}

pulse.rules = [
    {{
        matches = [
            {{ application.name = "~.*" }}
            {{ application.process.binary = "~.*" }}
        ]
        actions = {{
            update-props = {{
                channelmix.lock-volumes = true
                state.restore-props = false
                state.default-volume = 1.0
            }}
            quirks = [
                block-record-stream
                block-source-volume
                block-sink-volume
            ]
        }}
    }}
]
"#,
        remote_name = spa_string(OsStr::new(remote_name)),
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
    #[error("PIPEWIRE_REMOTE must name one socket in XDG_RUNTIME_DIR: {0:?}")]
    UnsafeRemote(String),
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
    #[error("playback frontend executable is unsafe: {0:?}")]
    UnsafeExecutable(PathBuf),
    #[error("cannot inspect playback frontend executable {path:?}: {source}")]
    ExecutableIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not start {role} at {path:?}: {source}")]
    StartProcess {
        role: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not inspect {role}: {source}")]
    InspectProcess {
        role: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{role} exited during startup: {status}")]
    ProcessExited {
        role: &'static str,
        status: ExitStatus,
    },
    #[error("{role} did not create {path:?} within five seconds")]
    StartTimedOut { role: &'static str, path: PathBuf },
    #[error("playback frontend I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn launch_scoped_frontend_is_direct_and_playback_only() {
        let pulse = pulse_configuration(
            "pipewire-0",
            Path::new("/run/user/1000/capsule/audio-test/pulse"),
            "0123456789abcdef0123456789abcdef",
        );

        assert!(pulse.contains("remote.name = \"pipewire-0\""));
        assert!(pulse.contains("client.access = restricted"));
        assert!(pulse.contains("pulse.allow-module-loading = false"));
        assert!(pulse.contains("block-record-stream"));
        assert!(pulse.contains("block-source-volume"));
        assert!(pulse.contains("block-sink-volume"));
        assert!(!pulse.contains("pulse-tunnel"));
        assert!(!pulse.contains("tunnel.mode"));
        assert!(!pulse.contains("Audio/Source"));
        assert!(!pulse.contains("target.object"));
        assert!(!pulse.contains("libpipewire-module-rt"));
    }

    #[test]
    fn frontend_configuration_escapes_paths_as_spa_strings() {
        let escaped = spa_string(OsStr::new("/run/user/a\\b\"c"));
        assert_eq!(escaped, r#""/run/user/a\\b\"c""#);
    }

    #[test]
    #[ignore = "requires a live PipeWire user audio graph and PulseAudio tools"]
    fn live_frontend_accepts_playback_and_denies_recording() {
        let broker = PlaybackBroker::start().unwrap();
        let runtime_dir = broker.runtime_dir.clone();
        UnixStream::connect(broker.socket()).unwrap();
        let server = format!("unix:{}", broker.socket().display());

        let info = Command::new("/usr/bin/pactl")
            .arg("info")
            .env("PULSE_SERVER", &server)
            .output()
            .unwrap();
        assert!(info.status.success());

        let mut recording = Command::new("/usr/bin/parec")
            .args(["--raw", "--rate=48000", "--channels=1", "--format=s16le"])
            .env("PULSE_SERVER", &server)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let record_status = loop {
            if let Some(status) = recording.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = recording.kill();
                let _ = recording.wait();
                panic!("recording stream was not rejected");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let mut record_error = String::new();
        recording
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut record_error)
            .unwrap();
        assert!(!record_status.success());
        assert!(
            record_error.contains("Access denied")
                || record_error.contains("Operation not permitted"),
            "unexpected recording failure: {record_error}"
        );

        let mut playback = Command::new("/usr/bin/paplay")
            .args([
                "--raw",
                "--rate=48000",
                "--channels=2",
                "--format=s16le",
                "/dev/zero",
            ])
            .env("PULSE_SERVER", server)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(500));
        assert!(playback.try_wait().unwrap().is_none());
        let _ = playback.kill();
        let _ = playback.wait();

        drop(broker);
        assert!(!runtime_dir.exists());
    }
}
