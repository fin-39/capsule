//! Single-file capsule storage planning.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use crate::backend::capabilities::{Capability, CapabilityReport};
use crate::backend::validate_host_absolute;
use crate::backend::{CommandSpec, PathValidationError, validate_capsule_relative};

const MIB: u64 = 1024 * 1024;
pub const MIN_IMAGE_SIZE_MIB: u64 = 64;

/// Safely resolve a capsule-relative path without permitting `..` traversal.
pub fn resolve_inside_capsule(
    capsule_root: &Path,
    relative: &Path,
) -> Result<PathBuf, StorageError> {
    validate_host_absolute(capsule_root)?;
    validate_capsule_relative(relative)?;
    Ok(capsule_root.join(relative))
}

/// A sparse ext4 image creation operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageCreatePlan {
    pub image_path: PathBuf,
    pub size_bytes: u64,
    pub format: CommandSpec,
}

impl ImageCreatePlan {
    pub fn new(
        image_path: impl Into<PathBuf>,
        size_mib: u64,
        capabilities: &CapabilityReport,
    ) -> Result<Self, StorageError> {
        let mkfs = capabilities
            .get(Capability::MkfsExt4)
            .ok_or(StorageError::MissingCapability(Capability::MkfsExt4))?;
        Self::with_formatter(image_path, size_mib, mkfs)
    }

    pub fn with_formatter(
        image_path: impl Into<PathBuf>,
        size_mib: u64,
        mkfs_ext4: impl Into<PathBuf>,
    ) -> Result<Self, StorageError> {
        let image_path = image_path.into();
        validate_image_path(&image_path)?;
        if size_mib < MIN_IMAGE_SIZE_MIB {
            return Err(StorageError::ImageTooSmall {
                requested_mib: size_mib,
                minimum_mib: MIN_IMAGE_SIZE_MIB,
            });
        }
        let size_bytes = size_mib
            .checked_mul(MIB)
            .ok_or(StorageError::ImageSizeOverflow(size_mib))?;

        // The image path is absolute and therefore cannot be parsed as an
        // option.  No shell is involved.
        let format = CommandSpec::new(mkfs_ext4)
            .arg("-q")
            .arg("-F")
            .arg("-m")
            .arg("0")
            // fuse2fs does not replay or update the ext4 journal. Creating one
            // would only produce misleading warnings; unclean-image recovery
            // with e2fsck remains an explicit supervisor roadmap item.
            .arg("-O")
            .arg("^has_journal")
            .arg("-L")
            .arg("CAPSULE")
            .arg(image_path.as_os_str());

        Ok(Self {
            image_path,
            size_bytes,
            format,
        })
    }

    /// Explicitly create and format the image. Existing files are never
    /// overwritten. A partially created image is removed if formatting fails.
    pub fn execute(&self) -> Result<(), StorageError> {
        let parent = self
            .image_path
            .parent()
            .ok_or_else(|| StorageError::MissingParent(self.image_path.clone()))?;
        if !parent.is_dir() {
            return Err(StorageError::MissingParent(parent.to_path_buf()));
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.image_path)
            .map_err(|source| io_error(&self.image_path, source))?;

        let prepare_result = file
            .set_len(self.size_bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| io_error(&self.image_path, source));
        drop(file);
        if let Err(error) = prepare_result {
            let _ = fs::remove_file(&self.image_path);
            return Err(error);
        }

        let status = match self.format.execute() {
            Ok(status) => status,
            Err(source) => {
                let _ = fs::remove_file(&self.image_path);
                return Err(io_error(&self.format.program, source));
            }
        };
        if !status.success() {
            let _ = fs::remove_file(&self.image_path);
            return Err(StorageError::FormatterFailed(status));
        }
        Ok(())
    }
}

/// Mount and matching unmount commands for an ext4 capsule image through FUSE.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageMountPlan {
    pub image_path: PathBuf,
    pub mount_point: PathBuf,
    pub mount: CommandSpec,
    pub unmount: CommandSpec,
}

impl ImageMountPlan {
    pub fn new(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
        capabilities: &CapabilityReport,
    ) -> Result<Self, StorageError> {
        let fuse2fs = capabilities
            .get(Capability::Fuse2fs)
            .ok_or(StorageError::MissingCapability(Capability::Fuse2fs))?;
        let fusermount = capabilities
            .get(Capability::Fusermount)
            .ok_or(StorageError::MissingCapability(Capability::Fusermount))?;
        Self::with_tools(image_path, mount_point, fuse2fs, fusermount)
    }

    /// Build the writable mount used while Wine is running.
    pub fn new_runnable(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
        capabilities: &CapabilityReport,
    ) -> Result<Self, StorageError> {
        let fuse2fs = capabilities
            .get(Capability::Fuse2fs)
            .ok_or(StorageError::MissingCapability(Capability::Fuse2fs))?;
        let fusermount = capabilities
            .get(Capability::Fusermount)
            .ok_or(StorageError::MissingCapability(Capability::Fusermount))?;
        Self::with_tools_and_options(
            image_path,
            mount_point,
            fuse2fs,
            fusermount,
            "rw,nosuid,nodev,noatime,fakeroot",
        )
    }

    /// Build a read-only mount plan for trusted metadata inspection.
    pub fn new_read_only(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
        capabilities: &CapabilityReport,
    ) -> Result<Self, StorageError> {
        let fuse2fs = capabilities
            .get(Capability::Fuse2fs)
            .ok_or(StorageError::MissingCapability(Capability::Fuse2fs))?;
        let fusermount = capabilities
            .get(Capability::Fusermount)
            .ok_or(StorageError::MissingCapability(Capability::Fusermount))?;
        Self::with_tools_and_options(
            image_path,
            mount_point,
            fuse2fs,
            fusermount,
            "ro,nosuid,nodev,noexec,noatime,fakeroot",
        )
    }

    pub fn with_tools(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
        fuse2fs: impl Into<PathBuf>,
        fusermount: impl Into<PathBuf>,
    ) -> Result<Self, StorageError> {
        Self::with_tools_and_options(
            image_path,
            mount_point,
            fuse2fs,
            fusermount,
            "rw,nosuid,nodev,noexec,noatime,fakeroot",
        )
    }

    fn with_tools_and_options(
        image_path: impl Into<PathBuf>,
        mount_point: impl Into<PathBuf>,
        fuse2fs: impl Into<PathBuf>,
        fusermount: impl Into<PathBuf>,
        mount_options: &'static str,
    ) -> Result<Self, StorageError> {
        let image_path = image_path.into();
        let mount_point = mount_point.into();
        validate_image_path(&image_path)?;
        validate_host_absolute(&mount_point)?;

        let mount = CommandSpec::new(fuse2fs)
            .arg("-o")
            .arg(mount_options)
            .arg(image_path.as_os_str())
            .arg(mount_point.as_os_str());
        let unmount = CommandSpec::new(fusermount)
            .arg("-u")
            .arg("--")
            .arg(mount_point.as_os_str());

        Ok(Self {
            image_path,
            mount_point,
            mount,
            unmount,
        })
    }

    /// Explicitly mount the image into an empty, owner-only directory.
    pub fn execute_mount(&self) -> Result<(), StorageError> {
        let metadata = fs::symlink_metadata(&self.image_path)
            .map_err(|source| io_error(&self.image_path, source))?;
        if !metadata.file_type().is_file() {
            return Err(StorageError::ImageNotRegular(self.image_path.clone()));
        }

        if self.mount_point.exists() {
            let metadata = fs::symlink_metadata(&self.mount_point)
                .map_err(|source| io_error(&self.mount_point, source))?;
            if !metadata.file_type().is_dir() {
                return Err(StorageError::UnsafeMountPoint(self.mount_point.clone()));
            }
            if fs::read_dir(&self.mount_point)
                .map_err(|source| io_error(&self.mount_point, source))?
                .next()
                .is_some()
            {
                return Err(StorageError::MountPointNotEmpty(self.mount_point.clone()));
            }
        } else {
            fs::create_dir(&self.mount_point)
                .map_err(|source| io_error(&self.mount_point, source))?;
        }
        fs::set_permissions(&self.mount_point, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_error(&self.mount_point, source))?;

        let status = self
            .mount
            .execute()
            .map_err(|source| io_error(&self.mount.program, source))?;
        if !status.success() {
            return Err(StorageError::MountFailed(status));
        }
        if !is_mountpoint(&self.mount_point)? {
            return Err(StorageError::MountVerificationFailed(
                self.mount_point.clone(),
            ));
        }
        Ok(())
    }

    pub fn execute_unmount(&self) -> Result<(), StorageError> {
        let status = self
            .unmount
            .execute()
            .map_err(|source| io_error(&self.unmount.program, source))?;
        if !status.success() {
            return Err(StorageError::UnmountFailed(status));
        }
        if is_mountpoint(&self.mount_point)? {
            return Err(StorageError::UnmountVerificationFailed(
                self.mount_point.clone(),
            ));
        }
        Ok(())
    }
}

fn is_mountpoint(path: &Path) -> Result<bool, StorageError> {
    let canonical = fs::canonicalize(path).map_err(|source| io_error(path, source))?;
    let mountinfo_path = Path::new("/proc/self/mountinfo");
    let mountinfo = fs::read(mountinfo_path).map_err(|source| io_error(mountinfo_path, source))?;
    let expected = canonical.as_os_str().as_bytes();

    Ok(mountinfo.split(|byte| *byte == b'\n').any(|line| {
        let Some(field) = line.split(|byte| *byte == b' ').nth(4) else {
            return false;
        };
        decode_mountinfo_field(field) == expected
    }))
}

fn decode_mountinfo_field(field: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\'
            && index + 3 < field.len()
            && matches!(field[index + 1], b'0'..=b'3')
            && field[index + 2..=index + 3]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'7'))
        {
            let value = (field[index + 1] - b'0') * 64
                + (field[index + 2] - b'0') * 8
                + (field[index + 3] - b'0');
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(field[index]);
            index += 1;
        }
    }
    decoded
}

pub fn validate_image_path(path: &Path) -> Result<(), StorageError> {
    validate_host_absolute(path)?;
    if path.extension().and_then(|extension| extension.to_str()) != Some("capsule") {
        return Err(StorageError::WrongImageExtension(path.to_path_buf()));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    InvalidPath(#[from] PathValidationError),
    #[error("capsule image must have a .capsule extension: {0:?}")]
    WrongImageExtension(PathBuf),
    #[error("capsule image must be at least {minimum_mib} MiB (requested {requested_mib} MiB)")]
    ImageTooSmall {
        requested_mib: u64,
        minimum_mib: u64,
    },
    #[error("capsule image size overflows for {0} MiB")]
    ImageSizeOverflow(u64),
    #[error("required backend capability is unavailable: {0:?}")]
    MissingCapability(Capability),
    #[error("capsule image parent directory does not exist: {0:?}")]
    MissingParent(PathBuf),
    #[error("capsule image is not a regular file: {0:?}")]
    ImageNotRegular(PathBuf),
    #[error("mount point is not a real directory: {0:?}")]
    UnsafeMountPoint(PathBuf),
    #[error("mount point must be empty: {0:?}")]
    MountPointNotEmpty(PathBuf),
    #[error("filesystem formatter exited unsuccessfully: {0}")]
    FormatterFailed(ExitStatus),
    #[error("filesystem mount exited unsuccessfully: {0}")]
    MountFailed(ExitStatus),
    #[error("filesystem helper returned success but no mount was found at {0:?}")]
    MountVerificationFailed(PathBuf),
    #[error("filesystem unmount exited unsuccessfully: {0}")]
    UnmountFailed(ExitStatus),
    #[error("filesystem helper returned success but the mount is still present at {0:?}")]
    UnmountVerificationFailed(PathBuf),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> StorageError {
    StorageError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn create_plan_is_sparse_ext4_and_never_uses_a_shell() {
        let plan = ImageCreatePlan::with_formatter(
            "/home/user/Games/My Game.capsule",
            1024,
            "/usr/bin/mkfs.ext4",
        )
        .unwrap();

        assert_eq!(plan.size_bytes, 1024 * MIB);
        assert_eq!(
            plan.format.argv(),
            [
                OsStr::new("/usr/bin/mkfs.ext4"),
                OsStr::new("-q"),
                OsStr::new("-F"),
                OsStr::new("-m"),
                OsStr::new("0"),
                OsStr::new("-O"),
                OsStr::new("^has_journal"),
                OsStr::new("-L"),
                OsStr::new("CAPSULE"),
                OsStr::new("/home/user/Games/My Game.capsule"),
            ]
        );
    }

    #[test]
    fn mount_plan_has_defensive_flags() {
        let plan = ImageMountPlan::with_tools(
            "/home/user/game.capsule",
            "/run/user/1000/capsule/mnt",
            "/usr/bin/fuse2fs",
            "/usr/bin/fusermount3",
        )
        .unwrap();
        let options = plan.mount.args[1].to_string_lossy();
        assert!(options.contains("nosuid"));
        assert!(options.contains("nodev"));
        assert!(options.contains("noexec"));

        let runnable = ImageMountPlan::with_tools_and_options(
            "/home/user/game.capsule",
            "/run/user/1000/capsule/run-mnt",
            "/usr/bin/fuse2fs",
            "/usr/bin/fusermount3",
            "rw,nosuid,nodev,noatime,fakeroot",
        )
        .unwrap();
        assert!(!runnable.mount.args[1].to_string_lossy().contains("noexec"));

        let read_only = ImageMountPlan::with_tools_and_options(
            "/home/user/game.capsule",
            "/run/user/1000/capsule/icon-mnt",
            "/usr/bin/fuse2fs",
            "/usr/bin/fusermount3",
            "ro,nosuid,nodev,noexec,noatime,fakeroot",
        )
        .unwrap();
        assert!(read_only.mount.args[1].to_string_lossy().starts_with("ro,"));
        assert!(read_only.mount.args[1].to_string_lossy().contains("noexec"));
    }

    #[test]
    fn rejects_unsafe_storage_paths() {
        assert!(
            ImageCreatePlan::with_formatter(
                "/home/user/../escape.capsule",
                64,
                "/usr/bin/mkfs.ext4"
            )
            .is_err()
        );
        assert!(
            ImageCreatePlan::with_formatter("relative.capsule", 64, "/usr/bin/mkfs.ext4").is_err()
        );
        assert!(resolve_inside_capsule(Path::new("/mnt/game"), Path::new("../secret")).is_err());
    }

    #[test]
    fn decodes_mountinfo_path_escapes() {
        assert_eq!(
            decode_mountinfo_field(br"/run/user/1000/My\040Game\134Root"),
            b"/run/user/1000/My Game\\Root"
        );
    }
}
