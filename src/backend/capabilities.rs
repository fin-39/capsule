//! Discovery of host tools used by the backend.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    Bubblewrap,
    Gamescope,
    Xwayland,
    XwaylandWrapper,
    WindowCenter,
    NetworkHelper,
    ClipboardGuard,
    WlPaste,
    Wine,
    WineServer,
    WineBoot,
    WinePath,
    Fuse2fs,
    MkfsExt4,
    SystemdRun,
    Sandwine,
    Slirp4netns,
    Nft,
    Curl,
    Fusermount,
}

impl Capability {
    pub const ALL: [Self; 20] = [
        Self::Bubblewrap,
        Self::Gamescope,
        Self::Xwayland,
        Self::XwaylandWrapper,
        Self::WindowCenter,
        Self::NetworkHelper,
        Self::ClipboardGuard,
        Self::WlPaste,
        Self::Wine,
        Self::WineServer,
        Self::WineBoot,
        Self::WinePath,
        Self::Fuse2fs,
        Self::MkfsExt4,
        Self::SystemdRun,
        Self::Sandwine,
        Self::Slirp4netns,
        Self::Nft,
        Self::Curl,
        Self::Fusermount,
    ];

    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bwrap",
            Self::Gamescope => "gamescope",
            Self::Xwayland => "Xwayland",
            Self::XwaylandWrapper => "capsule-xwayland",
            Self::WindowCenter => "capsule-window-center",
            Self::NetworkHelper => "capsule-network",
            Self::ClipboardGuard => "libcapsule.so",
            Self::WlPaste => "wl-paste",
            Self::Wine => "wine",
            Self::WineServer => "wineserver",
            Self::WineBoot => "wineboot",
            Self::WinePath => "winepath",
            Self::Fuse2fs => "fuse2fs",
            Self::MkfsExt4 => "mkfs.ext4",
            Self::SystemdRun => "systemd-run",
            Self::Sandwine => "sandwine",
            Self::Slirp4netns => "slirp4netns",
            Self::Nft => "nft",
            Self::Curl => "curl",
            Self::Fusermount => "fusermount3",
        }
    }

    pub const fn override_variable(self) -> &'static str {
        match self {
            Self::Bubblewrap => "CAPSULE_BUBBLEWRAP",
            Self::Gamescope => "CAPSULE_GAMESCOPE",
            Self::Xwayland => "CAPSULE_XWAYLAND",
            Self::XwaylandWrapper => "CAPSULE_XWAYLAND_WRAPPER",
            Self::WindowCenter => "CAPSULE_WINDOW_CENTER",
            Self::NetworkHelper => "CAPSULE_NETWORK_HELPER",
            Self::ClipboardGuard => "CAPSULE_CLIPBOARD_GUARD",
            Self::WlPaste => "CAPSULE_WL_PASTE",
            Self::Wine => "CAPSULE_WINE",
            Self::WineServer => "CAPSULE_WINESERVER",
            Self::WineBoot => "CAPSULE_WINEBOOT",
            Self::WinePath => "CAPSULE_WINEPATH",
            Self::Fuse2fs => "CAPSULE_FUSE2FS",
            Self::MkfsExt4 => "CAPSULE_MKFS_EXT4",
            Self::SystemdRun => "CAPSULE_SYSTEMD_RUN",
            Self::Sandwine => "CAPSULE_SANDWINE",
            Self::Slirp4netns => "CAPSULE_SLIRP4NETNS",
            Self::Nft => "CAPSULE_NFT",
            Self::Curl => "CAPSULE_CURL",
            Self::Fusermount => "CAPSULE_FUSERMOUNT",
        }
    }
}

/// Resolved executable paths.  Missing entries remain absent and must never be
/// silently replaced with an unsandboxed fallback.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilityReport {
    tools: BTreeMap<Capability, PathBuf>,
}

/// Short UI-facing name retained for the application doctor.
pub type Capabilities = CapabilityReport;

impl CapabilityReport {
    pub fn detect() -> Self {
        // Host-authority helpers must never be selected from a user-controlled
        // PATH entry. Arch and other merged-/usr systems provide these in
        // /usr/bin; /bin is retained for conventional layouts.
        Self::detect_in(OsStr::new("/usr/bin:/bin"))
    }

    pub fn detect_in(path: &OsStr) -> Self {
        let mut tools = BTreeMap::new();
        for capability in Capability::ALL {
            if let Some(executable) = find_executable(capability.executable_name(), path) {
                tools.insert(capability, executable);
            }
        }
        Self { tools }
    }

    pub fn get(&self, capability: Capability) -> Option<&Path> {
        self.tools.get(&capability).map(PathBuf::as_path)
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.tools.contains_key(&capability)
    }

    pub fn missing<'a>(
        &'a self,
        required: &'a [Capability],
    ) -> impl Iterator<Item = Capability> + 'a {
        required
            .iter()
            .copied()
            .filter(|capability| !self.has(*capability))
    }

    /// Add a trusted, explicitly selected executable (for example a bundled
    /// Sandwine virtual environment) after verifying that it is executable.
    pub fn insert_override(
        &mut self,
        capability: Capability,
        executable: impl Into<PathBuf>,
    ) -> Result<(), CapabilityError> {
        let executable = executable.into();
        if !executable.is_absolute() {
            return Err(CapabilityError::OverrideNotAbsolute(executable));
        }
        if !is_executable_file(&executable) {
            return Err(CapabilityError::OverrideNotExecutable(executable));
        }
        self.tools.insert(capability, executable);
        Ok(())
    }
}

impl fmt::Display for CapabilityReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, capability) in Capability::ALL.into_iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            let name = capability.executable_name();
            match self.get(capability) {
                Some(path) => write!(formatter, "✓ {name}: {}", path.display())?,
                None => write!(formatter, "✗ {name}: missing")?,
            }
        }
        Ok(())
    }
}

/// Apply an absolute Sandwine override supplied by the trusted launcher
/// configuration. Relative values are rejected instead of searched from the
/// current directory.
pub fn detect_with_environment_override() -> Result<CapabilityReport, CapabilityError> {
    let mut report = CapabilityReport::detect();
    for capability in Capability::ALL {
        if let Some(path) =
            env::var_os(capability.override_variable()).filter(|path| !path.is_empty())
        {
            report.insert_override(capability, PathBuf::from(path))?;
        }
    }
    if env::var_os(Capability::Sandwine.override_variable())
        .filter(|path| !path.is_empty())
        .is_none()
    {
        let development_tool = Path::new(env!("CARGO_MANIFEST_DIR")).join(".venv/bin/sandwine");
        if is_executable_file(&development_tool) {
            report.insert_override(Capability::Sandwine, development_tool)?;
        }
    }
    if env::var_os(Capability::XwaylandWrapper.override_variable())
        .filter(|path| !path.is_empty())
        .is_none()
        && let Some(development_tool) = sibling_development_tool("capsule-xwayland")
    {
        report.insert_override(Capability::XwaylandWrapper, development_tool)?;
    }
    if env::var_os(Capability::WindowCenter.override_variable())
        .filter(|path| !path.is_empty())
        .is_none()
        && let Some(development_tool) = sibling_development_tool("capsule-window-center")
    {
        report.insert_override(Capability::WindowCenter, development_tool)?;
    }
    if env::var_os(Capability::NetworkHelper.override_variable())
        .filter(|path| !path.is_empty())
        .is_none()
        && let Some(development_tool) = sibling_development_tool("capsule-network")
    {
        report.insert_override(Capability::NetworkHelper, development_tool)?;
    }
    if env::var_os(Capability::ClipboardGuard.override_variable())
        .filter(|path| !path.is_empty())
        .is_none()
        && let Some(development_tool) = sibling_development_tool("libcapsule.so")
    {
        report.insert_override(Capability::ClipboardGuard, development_tool)?;
    }
    Ok(report)
}

fn sibling_development_tool(name: &str) -> Option<PathBuf> {
    let current = env::current_exe().ok()?;
    let directory = current.parent()?;
    let direct = directory.join(name);
    if is_executable_file(&direct) {
        return Some(direct);
    }
    if directory.file_name() == Some(OsStr::new("deps")) {
        let sibling = directory.parent()?.join(name);
        if is_executable_file(&sibling) {
            return Some(sibling);
        }
    }
    None
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CapabilityError {
    #[error("capability override must be an absolute path: {0:?}")]
    OverrideNotAbsolute(PathBuf),
    #[error("capability override is not an executable regular file: {0:?}")]
    OverrideNotExecutable(PathBuf),
}

fn find_executable(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    env::split_paths(search_path)
        // Relative PATH entries make the result depend on process cwd and are
        // unsuitable for a trusted execution plan.
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn detects_only_executable_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let bwrap = temp.path().join("bwrap");
        File::create(&bwrap).unwrap();
        fs::set_permissions(&bwrap, fs::Permissions::from_mode(0o700)).unwrap();
        File::create(temp.path().join("wine")).unwrap();

        let report = CapabilityReport::detect_in(temp.path().as_os_str());
        assert_eq!(report.get(Capability::Bubblewrap), Some(bwrap.as_path()));
        assert!(!report.has(Capability::Wine));
    }

    #[test]
    fn ignores_relative_path_entries() {
        let report = CapabilityReport::detect_in(OsStr::new(".:relative"));
        assert_eq!(report, CapabilityReport::default());
    }

    #[test]
    fn reports_missing_requirements() {
        let report = CapabilityReport::default();
        let missing: Vec<_> = report
            .missing(&[Capability::Bubblewrap, Capability::Sandwine])
            .collect();
        assert_eq!(missing, [Capability::Bubblewrap, Capability::Sandwine]);
    }

    #[test]
    fn supports_explicit_bundled_tool() {
        let temp = tempfile::tempdir().unwrap();
        let sandwine = temp.path().join("sandwine");
        File::create(&sandwine).unwrap();
        fs::set_permissions(&sandwine, fs::Permissions::from_mode(0o700)).unwrap();

        let mut report = CapabilityReport::default();
        report
            .insert_override(Capability::Sandwine, sandwine.clone())
            .unwrap();
        assert_eq!(report.get(Capability::Sandwine), Some(sandwine.as_path()));
    }
}
