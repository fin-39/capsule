//! Side-effect-free backend planning primitives.
//!
//! Backend operations are represented as [`CommandSpec`] values first.  A
//! command is only started when a caller deliberately invokes
//! [`CommandSpec::execute`] or [`CommandSpec::spawn`].  Keeping planning and
//! execution separate makes it possible for the UI to show the effective
//! permissions before anything untrusted is run.

pub mod audio;
pub mod capabilities;
pub mod icon;
pub mod importer;
pub mod launcher;
pub mod pe_icon;
pub mod portable;
pub mod steam;
pub mod storage;
pub mod supervisor;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

/// An exact process invocation.  Arguments are never interpreted by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub clear_environment: bool,
    pub environment: BTreeMap<OsString, OsString>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            clear_environment: false,
            environment: BTreeMap::new(),
        }
    }

    pub fn arg(mut self, argument: impl Into<OsString>) -> Self {
        self.args.push(argument.into());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    pub fn clear_environment(mut self) -> Self {
        self.clear_environment = true;
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Materialize this specification as a standard-library command.
    pub fn to_command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(path) = &self.current_dir {
            command.current_dir(path);
        }
        if self.clear_environment {
            command.env_clear();
        }
        command.envs(&self.environment);
        command
    }

    /// Explicitly execute the command and wait for it to finish.
    pub fn execute(&self) -> io::Result<ExitStatus> {
        self.to_command().status()
    }

    /// Explicitly spawn the command without waiting for it.
    pub fn spawn(&self) -> io::Result<Child> {
        self.to_command().stdin(Stdio::null()).spawn()
    }

    /// Return an argv-style view useful for logging and tests.
    ///
    /// The returned values must still be escaped before being rendered as a
    /// shell-like string.  Execution should always use [`Self::to_command`].
    pub fn argv(&self) -> Vec<&OsStr> {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .collect()
    }
}

/// Validate a path that must remain below a capsule root.
pub(crate) fn validate_capsule_relative(path: &Path) -> Result<(), PathValidationError> {
    use std::path::Component;

    if path.as_os_str().is_empty() {
        return Err(PathValidationError::Empty);
    }
    if path.is_absolute() {
        return Err(PathValidationError::MustBeRelative(path.to_path_buf()));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PathValidationError::Traversal(path.to_path_buf()));
    }
    Ok(())
}

/// Validate an absolute host-side path used by a trusted backend operation.
pub(crate) fn validate_host_absolute(path: &Path) -> Result<(), PathValidationError> {
    use std::path::Component;

    if path.as_os_str().is_empty() {
        return Err(PathValidationError::Empty);
    }
    if !path.is_absolute() {
        return Err(PathValidationError::MustBeAbsolute(path.to_path_buf()));
    }
    if path == Path::new("/") {
        return Err(PathValidationError::RootNotAllowed);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(PathValidationError::Traversal(path.to_path_buf()));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PathValidationError {
    #[error("path is empty")]
    Empty,
    #[error("path must be relative to the capsule: {0:?}")]
    MustBeRelative(PathBuf),
    #[error("path must be absolute: {0:?}")]
    MustBeAbsolute(PathBuf),
    #[error("the filesystem root is not a valid target")]
    RootNotAllowed,
    #[error("path contains traversal or non-normal components: {0:?}")]
    Traversal(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_arguments_are_not_shell_parsed() {
        let command = CommandSpec::new("/usr/bin/example")
            .arg("two words")
            .arg("$(touch /tmp/never)")
            .arg("semi;colon");

        assert_eq!(
            command.argv(),
            [
                OsStr::new("/usr/bin/example"),
                OsStr::new("two words"),
                OsStr::new("$(touch /tmp/never)"),
                OsStr::new("semi;colon"),
            ]
        );
    }

    #[test]
    fn capsule_paths_reject_escape_attempts() {
        assert!(validate_capsule_relative(Path::new("Game/game.exe")).is_ok());
        assert!(validate_capsule_relative(Path::new("../host-file")).is_err());
        assert!(validate_capsule_relative(Path::new("Game/../host-file")).is_err());
        assert!(validate_capsule_relative(Path::new("/etc/passwd")).is_err());
        assert!(validate_capsule_relative(Path::new("")).is_err());
    }
}
