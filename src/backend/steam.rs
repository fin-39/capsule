//! Download and validation of Valve's Windows Steam bootstrap installer.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::backend::CommandSpec;
use crate::backend::capabilities::{Capability, CapabilityReport};

/// Target of the official “Install Steam” link on Valve's Steam about page.
pub const STEAM_INSTALLER_URL: &str =
    "https://cdn.fastly.steamstatic.com/client/installer/SteamSetup.exe";
pub const STEAM_INSTALLER_NAME: &str = "SteamSetup.exe";

const MIN_INSTALLER_SIZE: u64 = 256 * 1024;
const MAX_INSTALLER_SIZE: u64 = 100 * 1024 * 1024;

/// Download a fresh installer into Capsule's disposable cache.
///
/// Curl is resolved through Capsule's trusted capability report. Redirects
/// remain HTTPS-only, and the completed response is accepted only when it is
/// a plausibly-sized PE executable. The temporary download is atomically
/// renamed so an interrupted update cannot poison the cached installer.
pub fn download_installer(
    cache_dir: &Path,
    capabilities: &CapabilityReport,
) -> Result<PathBuf, SteamInstallerError> {
    let curl = capabilities
        .get(Capability::Curl)
        .ok_or(SteamInstallerError::MissingCurl)?;
    let installer_dir = cache_dir.join("installers/steam");
    fs::create_dir_all(&installer_dir).map_err(|source| io_error(&installer_dir, source))?;
    fs::set_permissions(&installer_dir, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&installer_dir, source))?;

    let temporary = tempfile::Builder::new()
        .prefix(".SteamSetup-")
        .suffix(".download")
        .tempfile_in(&installer_dir)
        .map_err(|source| io_error(&installer_dir, source))?;
    let temporary_path = temporary.path().to_path_buf();
    let status = CommandSpec::new(curl)
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "30",
            "--max-time",
            "300",
            "--max-filesize",
            "104857600",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(temporary_path.as_os_str())
        .arg(STEAM_INSTALLER_URL)
        .execute()
        .map_err(SteamInstallerError::StartCurl)?;
    if !status.success() {
        return Err(SteamInstallerError::DownloadFailed(status));
    }
    validate_installer(&temporary_path)?;

    let destination = installer_dir.join(STEAM_INSTALLER_NAME);
    let (_file, kept_path) = temporary
        .keep()
        .map_err(|error| io_error(&temporary_path, error.error))?;
    fs::rename(&kept_path, &destination).map_err(|source| io_error(&destination, source))?;
    Ok(destination)
}

pub fn validate_installer(path: &Path) -> Result<(), SteamInstallerError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(SteamInstallerError::UnsafeInstaller(path.to_path_buf()));
    }
    if !(MIN_INSTALLER_SIZE..=MAX_INSTALLER_SIZE).contains(&metadata.len()) {
        return Err(SteamInstallerError::UnexpectedSize(metadata.len()));
    }

    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut dos_header = [0_u8; 64];
    file.read_exact(&mut dos_header)
        .map_err(|source| io_error(path, source))?;
    if &dos_header[..2] != b"MZ" {
        return Err(SteamInstallerError::NotPortableExecutable);
    }
    let pe_offset = u32::from_le_bytes(dos_header[0x3c..0x40].try_into().unwrap()) as u64;
    if pe_offset < 64 || pe_offset.saturating_add(4) > metadata.len() {
        return Err(SteamInstallerError::NotPortableExecutable);
    }
    file.seek(SeekFrom::Start(pe_offset))
        .map_err(|source| io_error(path, source))?;
    let mut signature = [0_u8; 4];
    file.read_exact(&mut signature)
        .map_err(|source| io_error(path, source))?;
    if &signature != b"PE\0\0" {
        return Err(SteamInstallerError::NotPortableExecutable);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SteamInstallerError {
    #[error("curl is required to download the official Steam installer")]
    MissingCurl,
    #[error("could not start the Steam installer download: {0}")]
    StartCurl(#[source] io::Error),
    #[error("the official Steam installer download failed: {0}")]
    DownloadFailed(std::process::ExitStatus),
    #[error("Steam installer path is not a regular non-symlink file: {0:?}")]
    UnsafeInstaller(PathBuf),
    #[error("Steam installer has an unexpected size: {0} bytes")]
    UnexpectedSize(u64),
    #[error("Steam installer is not a Windows PE executable")]
    NotPortableExecutable,
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> SteamInstallerError {
    SteamInstallerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::symlink;

    use super::*;

    fn write_test_pe(path: &Path) {
        let mut file = File::create(path).unwrap();
        file.set_len(MIN_INSTALLER_SIZE).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"MZ").unwrap();
        file.seek(SeekFrom::Start(0x3c)).unwrap();
        file.write_all(&0x80_u32.to_le_bytes()).unwrap();
        file.seek(SeekFrom::Start(0x80)).unwrap();
        file.write_all(b"PE\0\0").unwrap();
    }

    #[test]
    fn accepts_a_bounded_pe_installer() {
        let temp = tempfile::tempdir().unwrap();
        let installer = temp.path().join(STEAM_INSTALLER_NAME);
        write_test_pe(&installer);

        validate_installer(&installer).unwrap();
    }

    #[test]
    fn rejects_non_pe_and_symlink_installers() {
        let temp = tempfile::tempdir().unwrap();
        let invalid = temp.path().join("invalid.exe");
        File::create(&invalid)
            .unwrap()
            .set_len(MIN_INSTALLER_SIZE)
            .unwrap();
        assert!(matches!(
            validate_installer(&invalid),
            Err(SteamInstallerError::NotPortableExecutable)
        ));

        let valid = temp.path().join("valid.exe");
        write_test_pe(&valid);
        let link = temp.path().join("linked.exe");
        symlink(&valid, &link).unwrap();
        assert!(matches!(
            validate_installer(&link),
            Err(SteamInstallerError::UnsafeInstaller(_))
        ));
    }

    #[test]
    fn installer_source_is_valves_https_endpoint() {
        assert!(STEAM_INSTALLER_URL.starts_with("https://"));
        assert!(STEAM_INSTALLER_URL.ends_with("/client/installer/SteamSetup.exe"));
        assert!(STEAM_INSTALLER_URL.contains("steamstatic.com"));
    }
}
