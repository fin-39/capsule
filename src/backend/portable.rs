//! Bounded import of portable Windows folders and archives.
//!
//! Import never executes source content. A selected source is inspected and
//! copied into a fixed `drive_c/Game` subtree of a newly-created image. Wine
//! creates the rest of its prefix later, inside the normal Capsule sandbox.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;
use rustix::fs::{
    CWD, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, Stat, fstat, openat2,
    renameat_with,
};
use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_setfd};
use serde::Serialize;
use uuid::Uuid;
use zip::CompressionMethod;
use zip::read::ZipArchive;

use crate::backend::capabilities::CapabilityReport;
use crate::backend::storage::{ImageCreatePlan, ImageMountPlan, StorageError};
use crate::backend::validate_host_absolute;
use crate::model::RunnerKind;

pub const PORTABLE_GAME_ROOT: &str = "drive_c/Game";

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const ZIP_EOCD_MIN: usize = 22;
const ZIP_EOCD_WINDOW: u64 = 65_557;
const ZIP_EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const ZIP_CENTRAL_SIGNATURE: &[u8; 4] = b"PK\x01\x02";
const ARCHIVE_LISTING_LIMIT: u64 = 64 * MIB;
const ARCHIVE_DIAGNOSTIC_LIMIT: u64 = MIB;
const ARCHIVE_INSPECTION_SECONDS: u64 = 120;
const ARCHIVE_EXTRACTION_SECONDS: u64 = 7_200;
const ARCHIVE_MEMORY_LIMIT: u64 = 2 * GIB;
const ARCHIVE_PROCESS_LIMIT: u64 = 16;
const ARCHIVE_TMPFS_LIMIT: u64 = 256 * MIB;
const MAX_ARCHIVE_PARTS: u32 = 128;
const MIN_PORTABLE_IMAGE_MIB: u64 = 1_024;
const MIN_PORTABLE_HEADROOM_MIB: u64 = 1_024;
const PORTABLE_IMAGE_GRANULARITY_MIB: u64 = 256;

const BWRAP_PATH: &str = "/usr/bin/bwrap";
const TIMEOUT_PATH: &str = "/usr/bin/timeout";
const PRLIMIT_PATH: &str = "/usr/bin/prlimit";
const SEVEN_ZIP_PATH: &str = "/usr/lib/7zip/7z";
const SEVEN_ZIP_PLUGIN_PATH: &str = "/usr/lib/7zip/7z.so";
const SEVEN_ZIP_SIGNATURE: &[u8; 6] = b"7z\xbc\xaf'\x1c";
const RAR_SIGNATURE: &[u8; 6] = b"Rar!\x1a\x07";
const SEVEN_ZIP_RUNTIME_FILES: &[(&str, &str)] = &[
    ("/usr/lib/libsmartcols.so.1", "/usr/lib/libsmartcols.so.1"),
    ("/usr/lib/libstdc++.so.6", "/usr/lib/libstdc++.so.6"),
    ("/usr/lib/libgcc_s.so.1", "/usr/lib/libgcc_s.so.1"),
    ("/usr/lib/libc.so.6", "/usr/lib/libc.so.6"),
    ("/usr/lib/libm.so.6", "/usr/lib/libm.so.6"),
    (
        "/usr/lib64/ld-linux-x86-64.so.2",
        "/lib64/ld-linux-x86-64.so.2",
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PortableSource {
    Directory(PathBuf),
    Zip(PathBuf),
    /// An archive handled by the sandboxed full 7-Zip engine, including
    /// numbered 7z/ZIP sets and both modern and legacy multipart RAR naming.
    /// `PathBuf` is the part selected by the user; sibling parts are resolved
    /// once and held open throughout inspection and import.
    Archive(PathBuf),
}

impl PortableSource {
    pub fn path(&self) -> &Path {
        match self {
            Self::Directory(path) | Self::Zip(path) | Self::Archive(path) => path,
        }
    }

    fn kind_name(&self) -> &'static str {
        match self {
            Self::Directory(_) => "directory",
            Self::Zip(_) => "zip",
            Self::Archive(_) => "archive",
        }
    }
}

/// Turn any file selected in the archive picker into the sandboxed full 7-Zip
/// source. The strict in-process ZIP path remains available to callers and
/// tests, but the UI uses one worker for ZIP64, ZIPX and multipart support.
pub fn portable_archive_source(path: PathBuf) -> PortableSource {
    PortableSource::Archive(path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportLimits {
    pub max_entries: u64,
    pub max_executable_candidates: u64,
    pub max_depth: usize,
    pub max_component_bytes: usize,
    pub max_path_bytes: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_zip_bytes: u64,
    pub max_central_directory_bytes: u64,
    pub max_compression_ratio: u64,
    pub free_space_margin_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_executable_candidates: 512,
            max_depth: 64,
            max_component_bytes: 255,
            max_path_bytes: 1_024,
            max_total_bytes: 24 * GIB,
            max_file_bytes: 16 * GIB,
            max_zip_bytes: 16 * GIB,
            max_central_directory_bytes: 64 * MIB,
            max_compression_ratio: 1_000,
            free_space_margin_bytes: 2 * GIB,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortableInspection {
    pub suggested_name: String,
    pub executable_candidates: Vec<PathBuf>,
    /// Runner corresponding to each path in `executable_candidates`.
    pub candidate_runners: Vec<RunnerKind>,
    pub recommended_candidate: usize,
    pub entries: u64,
    pub uncompressed_bytes: u64,
}

impl PortableInspection {
    pub fn candidate(&self, index: usize) -> Option<(&Path, RunnerKind)> {
        self.executable_candidates
            .get(index)
            .zip(self.candidate_runners.get(index).copied())
            .map(|(path, runner)| (path.as_path(), runner))
    }
}

#[derive(Clone, Debug)]
pub struct PortableImportRequest {
    pub id: Uuid,
    pub name: String,
    pub source: PortableSource,
    /// Ephemeral archive credential. It is never written to the capsule
    /// manifest, library, command line, or diagnostic output.
    pub archive_password: Option<ArchivePassword>,
    pub image_path: PathBuf,
    pub image_size_mib: u64,
    pub runtime_root: PathBuf,
    pub limits: ImportLimits,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ArchivePassword(String);

impl ArchivePassword {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for ArchivePassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArchivePassword([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortableImportResult {
    pub inspection: PortableInspection,
}

/// Choose an ext4 capacity from the validated, uncompressed payload.
///
/// At least one quarter of the image remains available for filesystem
/// metadata, Wine initialization and game saves. Small games receive a 1 GiB
/// minimum allowance, and the visible capacity is rounded to 256 MiB steps.
pub fn recommended_image_size_mib(uncompressed_bytes: u64) -> u64 {
    let payload_mib = uncompressed_bytes.saturating_add(MIB - 1) / MIB;
    let headroom_mib = payload_mib
        .saturating_add(2)
        .checked_div(3)
        .unwrap_or_default()
        .max(MIN_PORTABLE_HEADROOM_MIB);
    let needed_mib = payload_mib
        .saturating_add(headroom_mib)
        .max(MIN_PORTABLE_IMAGE_MIB);
    let remainder = needed_mib % PORTABLE_IMAGE_GRANULARITY_MIB;
    if remainder == 0 {
        needed_mib
    } else {
        needed_mib.saturating_add(PORTABLE_IMAGE_GRANULARITY_MIB - remainder)
    }
}

pub fn inspect_portable_source(
    source: &PortableSource,
    limits: &ImportLimits,
) -> Result<PortableInspection, PortableImportError> {
    inspect_portable_source_with_password(source, limits, None)
}

pub fn inspect_portable_source_with_password(
    source: &PortableSource,
    limits: &ImportLimits,
    archive_password: Option<&ArchivePassword>,
) -> Result<PortableInspection, PortableImportError> {
    validate_host_absolute(source.path())?;
    let mut inspection = match source {
        PortableSource::Directory(path) => inspect_directory(path, limits, None)?,
        PortableSource::Zip(path) => inspect_zip(path, limits, None)?,
        PortableSource::Archive(path) => {
            inspect_external_archive(path, limits, None, archive_password)?
        }
    };
    if inspection.executable_candidates.is_empty() {
        return Err(PortableImportError::NoWindowsExecutables);
    }
    inspection.recommended_candidate = recommend_candidate(&inspection);
    Ok(inspection)
}

pub fn import_portable_game(
    request: &PortableImportRequest,
    capabilities: &CapabilityReport,
) -> Result<PortableImportResult, PortableImportError> {
    validate_host_absolute(request.source.path())?;
    validate_host_absolute(&request.image_path)?;
    validate_host_absolute(&request.runtime_root)?;
    if request.image_path.exists() {
        return Err(PortableImportError::DestinationExists(
            request.image_path.clone(),
        ));
    }

    let image_parent = request
        .image_path
        .parent()
        .ok_or_else(|| PortableImportError::MissingImageParent(request.image_path.clone()))?;
    reject_source_output_overlap(&request.source, image_parent, &request.runtime_root)?;
    fs::create_dir_all(image_parent).map_err(|source| io_error(image_parent, source))?;
    fs::create_dir_all(&request.runtime_root)
        .map_err(|source| io_error(&request.runtime_root, source))?;
    fs::set_permissions(&request.runtime_root, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&request.runtime_root, source))?;

    let inspection = inspect_portable_source_with_password(
        &request.source,
        &request.limits,
        request.archive_password.as_ref(),
    )?;
    let image_bytes = request
        .image_size_mib
        .checked_mul(MIB)
        .ok_or(PortableImportError::ImageSizeOverflow)?;
    let payload_budget = image_bytes.saturating_mul(3) / 4;
    if inspection.uncompressed_bytes > payload_budget {
        return Err(PortableImportError::PayloadDoesNotFit {
            bytes: inspection.uncompressed_bytes,
            budget: payload_budget,
        });
    }
    let required_space = inspection
        .uncompressed_bytes
        .checked_add(request.limits.free_space_margin_bytes)
        .ok_or(PortableImportError::SizeOverflow)?;
    let available =
        fs2::available_space(image_parent).map_err(|source| io_error(image_parent, source))?;
    if available < required_space {
        return Err(PortableImportError::InsufficientSpace {
            required: required_space,
            available,
        });
    }

    let final_name = request
        .image_path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| PortableImportError::InvalidImageName(request.image_path.clone()))?;
    let partial_image = image_parent.join(format!(
        ".{final_name}.{}.partial.capsule",
        Uuid::new_v4().simple()
    ));
    let run_dir = request.runtime_root.join(format!(
        "import-{}-{}-{}",
        request.id.simple(),
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let mount_point = run_dir.join("root");
    fs::create_dir(&run_dir).map_err(|source| io_error(&run_dir, source))?;
    fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(&run_dir, source))?;

    let create = ImageCreatePlan::new(&partial_image, request.image_size_mib, capabilities)?;
    if let Err(error) = create.execute() {
        let _ = fs::remove_dir(&run_dir);
        return Err(error.into());
    }

    let mount = ImageMountPlan::new(&partial_image, &mount_point, capabilities)?;
    if let Err(error) = mount.execute_mount() {
        let detached = match fs::remove_dir(&mount_point) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if detached {
            let _ = fs::remove_dir(&run_dir);
            let _ = fs::remove_file(&partial_image);
        }
        return Err(error.into());
    }

    let populate_result = populate_portable_image(request, &mount_point);
    let unmount_result = mount.execute_unmount();
    if unmount_result.is_ok() {
        let _ = fs::remove_dir_all(&run_dir);
    }

    let copied = match (populate_result, unmount_result) {
        (Ok(copied), Ok(())) => copied,
        (Err(error), Ok(())) => {
            let _ = fs::remove_file(&partial_image);
            return Err(error);
        }
        (_, Err(error)) => return Err(error.into()),
    };

    if copied.executable_candidates.is_empty() {
        let _ = fs::remove_file(&partial_image);
        return Err(PortableImportError::NoWindowsExecutables);
    }
    File::open(&partial_image)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(&partial_image, source))?;
    if let Err(source) = renameat_with(
        CWD,
        &partial_image,
        CWD,
        &request.image_path,
        RenameFlags::NOREPLACE,
    ) {
        let _ = fs::remove_file(&partial_image);
        return Err(io_error(&request.image_path, source.into()));
    }
    if let Ok(parent) = File::open(image_parent) {
        let _ = parent.sync_all();
    }

    Ok(PortableImportResult { inspection: copied })
}

fn populate_portable_image(
    request: &PortableImportRequest,
    root: &Path,
) -> Result<PortableInspection, PortableImportError> {
    let prefix = root.join("prefix");
    let game = prefix.join(PORTABLE_GAME_ROOT);
    let metadata = root.join(".capsule");
    for directory in [
        &prefix,
        &prefix.join("drive_c"),
        &game,
        &root.join("home"),
        &root.join("logs"),
        &metadata,
    ] {
        fs::create_dir(directory).map_err(|source| io_error(directory, source))?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
            .map_err(|source| io_error(directory, source))?;
    }
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(root, source))?;

    let mut inspection = match &request.source {
        PortableSource::Directory(path) => inspect_directory(path, &request.limits, Some(&game))?,
        PortableSource::Zip(path) => inspect_zip(path, &request.limits, Some(&game))?,
        PortableSource::Archive(path) => inspect_external_archive(
            path,
            &request.limits,
            Some(&game),
            request.archive_password.as_ref(),
        )?,
    };
    inspection.recommended_candidate = recommend_candidate(&inspection);

    let manifest_path = metadata.join("manifest.json");
    let manifest = PortableManifest {
        format_version: 1,
        id: request.id,
        name: &request.name,
        source_kind: request.source.kind_name(),
        entries: inspection.entries,
        uncompressed_bytes: inspection.uncompressed_bytes,
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .map_err(|source| io_error(&manifest_path, source))?;
    serde_json::to_writer_pretty(&mut file, &manifest).map_err(PortableImportError::Manifest)?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|source| io_error(&manifest_path, source))?;
    Ok(inspection)
}

#[derive(Serialize)]
struct PortableManifest<'a> {
    format_version: u32,
    id: Uuid,
    name: &'a str,
    source_kind: &'a str,
    entries: u64,
    uncompressed_bytes: u64,
}

struct ScanState {
    suggested_name: String,
    candidates: Vec<(PathBuf, RunnerKind)>,
    entries: u64,
    bytes: u64,
    windows_paths: BTreeMap<String, bool>,
    explicit_paths: BTreeSet<String>,
    files: BTreeMap<String, u64>,
}

impl ScanState {
    fn new(suggested_name: String) -> Self {
        Self {
            suggested_name,
            candidates: Vec::new(),
            entries: 0,
            bytes: 0,
            windows_paths: BTreeMap::new(),
            explicit_paths: BTreeSet::new(),
            files: BTreeMap::new(),
        }
    }

    fn finish(self) -> PortableInspection {
        let (executable_candidates, candidate_runners) = self.candidates.into_iter().unzip();
        PortableInspection {
            suggested_name: self.suggested_name,
            executable_candidates,
            candidate_runners,
            recommended_candidate: 0,
            entries: self.entries,
            uncompressed_bytes: self.bytes,
        }
    }

    fn add_entry(
        &mut self,
        relative: &Path,
        is_directory: bool,
        size: u64,
        limits: &ImportLimits,
    ) -> Result<String, PortableImportError> {
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or(PortableImportError::SizeOverflow)?;
        if self.entries > limits.max_entries {
            return Err(PortableImportError::TooManyEntries(limits.max_entries));
        }
        if size > limits.max_file_bytes {
            return Err(PortableImportError::FileTooLarge {
                path: relative.to_path_buf(),
                bytes: size,
                limit: limits.max_file_bytes,
            });
        }
        self.bytes = self
            .bytes
            .checked_add(size)
            .ok_or(PortableImportError::SizeOverflow)?;
        if self.bytes > limits.max_total_bytes {
            return Err(PortableImportError::PayloadTooLarge {
                bytes: self.bytes,
                limit: limits.max_total_bytes,
            });
        }
        let normalized = validate_windows_relative(relative, limits)?;
        register_windows_path(
            &normalized,
            is_directory,
            true,
            &mut self.windows_paths,
            &mut self.explicit_paths,
        )?;
        if !is_directory {
            self.files.insert(normalized.clone(), size);
        }
        Ok(normalized)
    }

    fn add_candidate(
        &mut self,
        candidate: PathBuf,
        runner: RunnerKind,
        limits: &ImportLimits,
    ) -> Result<(), PortableImportError> {
        if self.candidates.len() as u64 >= limits.max_executable_candidates {
            return Err(PortableImportError::TooManyExecutableCandidates(
                limits.max_executable_candidates,
            ));
        }
        self.candidates.push((candidate, runner));
        Ok(())
    }
}

fn inspect_directory(
    source: &Path,
    limits: &ImportLimits,
    destination: Option<&Path>,
) -> Result<PortableInspection, PortableImportError> {
    Ok(scan_directory(source, limits, destination, false)?.finish())
}

fn scan_directory(
    source: &Path,
    limits: &ImportLimits,
    destination: Option<&Path>,
    repair_native_modes: bool,
) -> Result<ScanState, PortableImportError> {
    let root = open_root_directory(source)?;
    let root_before = fstat(&root).map_err(|error| io_error(source, error.into()))?;
    let name = suggested_name(source);
    let mut state = ScanState::new(name);
    walk_directory_fd(
        &root,
        Path::new(""),
        destination,
        repair_native_modes,
        limits,
        &mut state,
    )?;
    let root_after = fstat(&root).map_err(|error| io_error(source, error.into()))?;
    if !same_identity_and_times(&root_before, &root_after) {
        return Err(PortableImportError::SourceChanged(source.to_path_buf()));
    }
    state.candidates.sort();
    Ok(state)
}

fn walk_directory_fd(
    directory_fd: &OwnedFd,
    relative_directory: &Path,
    destination: Option<&Path>,
    repair_native_modes: bool,
    limits: &ImportLimits,
    state: &mut ScanState,
) -> Result<(), PortableImportError> {
    let directory_before =
        fstat(directory_fd).map_err(|error| io_error(relative_directory, error.into()))?;
    let mut directory =
        Dir::read_from(directory_fd).map_err(|error| io_error(relative_directory, error.into()))?;
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(|error| io_error(relative_directory, error.into()))?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name_bytes);
        let relative = relative_directory.join(name);
        validate_windows_relative(&relative, limits)?;
        let child = openat2(
            directory_fd,
            entry.file_name(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_XDEV,
        )
        .map_err(|error| io_error(&relative, error.into()))?;
        let before = fstat(&child).map_err(|error| io_error(&relative, error.into()))?;
        match FileType::from_raw_mode(before.st_mode) {
            FileType::Directory => {
                state.add_entry(&relative, true, 0, limits)?;
                let destination_directory = destination.map(|root| root.join(&relative));
                if let Some(path) = &destination_directory {
                    fs::create_dir(path).map_err(|source| io_error(path, source))?;
                    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                        .map_err(|source| io_error(path, source))?;
                }
                walk_directory_fd(
                    &child,
                    &relative,
                    destination,
                    repair_native_modes,
                    limits,
                    state,
                )?;
            }
            FileType::RegularFile => {
                if before.st_nlink > 1 {
                    return Err(PortableImportError::HardLink(relative));
                }
                if before.st_size < 0 {
                    return Err(PortableImportError::InvalidFileSize(relative));
                }
                let size = before.st_size as u64;
                state.add_entry(&relative, false, size, limits)?;
                let is_candidate = has_launcher_extension(&relative);
                let mut source_file = File::from(child);
                let mut prefix = [0_u8; 4];
                let mut runner = None;
                let mut repaired_mode = None;
                if let Some(root) = destination {
                    let destination_file = root.join(&relative);
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&destination_file)
                        .map_err(|source| io_error(&destination_file, source))?;
                    let copied = copy_bounded(
                        &mut source_file,
                        &mut output,
                        size,
                        limits.max_file_bytes,
                        &relative,
                        Some(prefix.as_mut_slice()),
                    )?;
                    if copied != size {
                        return Err(PortableImportError::SourceChanged(relative));
                    }
                    output
                        .sync_all()
                        .map_err(|source| io_error(&destination_file, source))?;
                    let prefix_length = copied.min(prefix.len() as u64) as usize;
                    let output_mode =
                        portable_file_mode(before.st_mode as u32, &prefix[..prefix_length]);
                    output
                        .set_permissions(fs::Permissions::from_mode(output_mode))
                        .map_err(|source| io_error(&destination_file, source))?;
                    runner = is_candidate
                        .then(|| classify_launcher(&relative, &prefix))
                        .flatten();
                } else if is_candidate || repair_native_modes {
                    let read = source_file
                        .read(&mut prefix)
                        .map_err(|source| io_error(&relative, source))?;
                    if is_candidate {
                        runner = classify_launcher(&relative, &prefix[..read]);
                    }
                    if repair_native_modes {
                        repaired_mode =
                            Some(portable_file_mode(before.st_mode as u32, &prefix[..read]));
                    }
                }
                let after =
                    fstat(&source_file).map_err(|error| io_error(&relative, error.into()))?;
                if !same_identity_and_times(&before, &after) {
                    return Err(PortableImportError::SourceChanged(relative));
                }
                if let Some(mode) = repaired_mode {
                    source_file
                        .set_permissions(fs::Permissions::from_mode(mode))
                        .map_err(|source| io_error(&relative, source))?;
                }
                if let Some(runner) = runner {
                    state.add_candidate(
                        Path::new(PORTABLE_GAME_ROOT).join(&relative),
                        runner,
                        limits,
                    )?;
                }
            }
            _ => return Err(PortableImportError::UnsupportedFileType(relative)),
        }
    }
    let directory_after =
        fstat(directory_fd).map_err(|error| io_error(relative_directory, error.into()))?;
    if !same_identity_and_times(&directory_before, &directory_after) {
        return Err(PortableImportError::SourceChanged(
            relative_directory.to_path_buf(),
        ));
    }
    Ok(())
}

fn inspect_zip(
    source: &Path,
    limits: &ImportLimits,
    destination: Option<&Path>,
) -> Result<PortableInspection, PortableImportError> {
    let (file, before) = open_zip(source, limits)?;
    let preflight = preflight_zip(&file, source, limits)?;
    let mut archive = ZipArchive::new(file).map_err(PortableImportError::Zip)?;
    if archive.len() as u64 != preflight.entries {
        return Err(PortableImportError::MalformedZip(
            "central-directory entry count changed".into(),
        ));
    }
    let mut state = ScanState::new(suggested_name(source));
    let mut manifest = Vec::with_capacity(archive.len());

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(PortableImportError::Zip)?;
        if entry.encrypted() {
            return Err(PortableImportError::EncryptedZipEntry(index));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(PortableImportError::UnsupportedCompression {
                index,
                method: format!("{:?}", entry.compression()),
            });
        }
        if entry.is_symlink() {
            return Err(PortableImportError::ZipLink(index));
        }
        let raw_name = std::str::from_utf8(entry.name_raw())
            .map_err(|_| PortableImportError::NonUtf8ZipPath(index))?;
        if raw_name.contains('\\') {
            return Err(PortableImportError::UnsafeWindowsPath(raw_name.into()));
        }
        let is_directory = entry.is_dir();
        let trimmed = if is_directory {
            raw_name.trim_end_matches('/')
        } else {
            raw_name
        };
        if trimmed.is_empty() || trimmed.split('/').any(str::is_empty) {
            return Err(PortableImportError::UnsafeWindowsPath(raw_name.into()));
        }
        let relative = PathBuf::from(trimmed);
        if entry.enclosed_name().is_none() {
            return Err(PortableImportError::UnsafeWindowsPath(raw_name.into()));
        }
        let archived_mode =
            entry
                .unix_mode()
                .unwrap_or(if is_directory { 0o040755 } else { 0o100644 });
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            let expected = if is_directory { 0o040000 } else { 0o100000 };
            if kind != 0 && kind != expected {
                return Err(PortableImportError::UnsupportedZipFileType {
                    path: relative,
                    mode,
                    index,
                });
            }
        }
        let size = if is_directory { 0 } else { entry.size() };
        if !is_directory && entry.compressed_size() == 0 && size > 0 {
            return Err(PortableImportError::CompressionRatioExceeded(relative));
        }
        if !is_directory
            && entry.compressed_size() > 0
            && size
                > entry
                    .compressed_size()
                    .saturating_mul(limits.max_compression_ratio)
        {
            return Err(PortableImportError::CompressionRatioExceeded(relative));
        }
        state.add_entry(&relative, is_directory, size, limits)?;
        let is_candidate = !is_directory && has_launcher_extension(&relative);
        let mut prefix = [0_u8; 4];
        let runner = if is_candidate {
            let read = entry
                .read(&mut prefix)
                .map_err(|source| io_error(&relative, source))?;
            classify_launcher(&relative, &prefix[..read])
        } else {
            None
        };
        if let Some(runner) = runner {
            state.add_candidate(
                Path::new(PORTABLE_GAME_ROOT).join(&relative),
                runner,
                limits,
            )?;
        }
        manifest.push(ZipManifestEntry {
            index,
            relative,
            is_directory,
            size,
            mode: archived_mode,
        });
    }
    validate_manifest_prefix_conflicts(&state.windows_paths)?;

    if let Some(destination) = destination {
        let mut actual_total = 0_u64;
        for item in &manifest {
            let output_path = destination.join(&item.relative);
            if item.is_directory {
                if !output_path.exists() {
                    fs::create_dir(&output_path)
                        .map_err(|source| io_error(&output_path, source))?;
                }
                fs::set_permissions(&output_path, fs::Permissions::from_mode(0o755))
                    .map_err(|source| io_error(&output_path, source))?;
                continue;
            }
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
            }
            let mut entry = archive
                .by_index(item.index)
                .map_err(PortableImportError::Zip)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&output_path)
                .map_err(|source| io_error(&output_path, source))?;
            let mut prefix = [0_u8; 4];
            let copied = copy_bounded(
                &mut entry,
                &mut output,
                item.size,
                limits.max_file_bytes,
                &item.relative,
                Some(prefix.as_mut_slice()),
            )?;
            if copied != item.size {
                return Err(PortableImportError::ZipSizeMismatch {
                    path: item.relative.clone(),
                    declared: item.size,
                    actual: copied,
                });
            }
            actual_total = actual_total
                .checked_add(copied)
                .ok_or(PortableImportError::SizeOverflow)?;
            let prefix_length = copied.min(prefix.len() as u64) as usize;
            output
                .set_permissions(fs::Permissions::from_mode(portable_file_mode(
                    item.mode,
                    &prefix[..prefix_length],
                )))
                .map_err(|source| io_error(&output_path, source))?;
            if actual_total > limits.max_total_bytes {
                return Err(PortableImportError::PayloadTooLarge {
                    bytes: actual_total,
                    limit: limits.max_total_bytes,
                });
            }
            output
                .sync_all()
                .map_err(|source| io_error(&output_path, source))?;
        }
        if actual_total != state.bytes {
            return Err(PortableImportError::ZipTotalSizeMismatch {
                declared: state.bytes,
                actual: actual_total,
            });
        }
    }

    let file = archive.into_inner();
    let after = fstat(&file).map_err(|error| io_error(source, error.into()))?;
    if !same_identity_and_times(&before, &after) {
        return Err(PortableImportError::SourceChanged(source.to_path_buf()));
    }
    state.candidates.sort();
    Ok(state.finish())
}

#[derive(Debug)]
struct ResolvedArchive {
    primary: PathBuf,
    parts: Vec<PathBuf>,
    suggested_name: String,
    signature: Option<ArchiveSignature>,
    equal_sized_volumes: bool,
    nested_tar: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveSignature {
    SevenZip,
    Rar,
}

impl ArchiveSignature {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::SevenZip => SEVEN_ZIP_SIGNATURE,
            Self::Rar => RAR_SIGNATURE,
        }
    }
}

fn resolve_archive_parts(selected: &Path) -> Result<ResolvedArchive, PortableImportError> {
    validate_host_absolute(selected)?;
    let parent = selected
        .parent()
        .ok_or_else(|| PortableImportError::UnsupportedArchiveName(selected.to_path_buf()))?;
    let selected_name = selected
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| PortableImportError::NonUtf8ArchiveName(selected.to_path_buf()))?;
    if let Some((base, prefix, suffix, width, selected_index)) = parse_part_rar_name(selected_name)
    {
        return resolve_numbered_archive(
            parent,
            base,
            prefix,
            suffix,
            width,
            selected_index,
            Some(ArchiveSignature::Rar),
            false,
        );
    }
    if let Some(resolved) = resolve_legacy_rar_set(parent, selected_name)? {
        return Ok(resolved);
    }
    if let Some(resolved) = resolve_classic_zip_set(parent, selected_name)? {
        return Ok(resolved);
    }
    if let Some((archive_name, prefix, width, selected_index)) =
        parse_trailing_numbered_volume(selected_name)
    {
        return resolve_numbered_archive(
            parent,
            suggested_archive_name(archive_name),
            prefix,
            String::new(),
            width,
            selected_index,
            archive_signature_for_name(archive_name),
            is_compressed_tar_name(archive_name),
        );
    }

    Ok(ResolvedArchive {
        primary: selected.to_path_buf(),
        parts: vec![selected.to_path_buf()],
        suggested_name: suggested_archive_name(selected_name),
        signature: archive_signature_for_name(selected_name),
        equal_sized_volumes: false,
        nested_tar: is_compressed_tar_name(selected_name),
    })
}

fn resolve_numbered_archive(
    parent: &Path,
    suggested_name: String,
    prefix: String,
    suffix: String,
    width: usize,
    selected_index: u32,
    signature: Option<ArchiveSignature>,
    nested_tar: bool,
) -> Result<ResolvedArchive, PortableImportError> {
    if selected_index == 0 || selected_index > MAX_ARCHIVE_PARTS {
        return Err(PortableImportError::TooManyArchiveVolumes(
            MAX_ARCHIVE_PARTS,
        ));
    }
    let numbered = collect_numbered_archive_siblings(parent, &prefix, &suffix, width)?;
    let maximum = numbered
        .keys()
        .copied()
        .max()
        .unwrap_or(selected_index)
        .max(selected_index);
    if maximum > MAX_ARCHIVE_PARTS {
        return Err(PortableImportError::TooManyArchiveVolumes(
            MAX_ARCHIVE_PARTS,
        ));
    }
    let mut parts = Vec::with_capacity(maximum as usize);
    for index in 1..=maximum {
        let expected = parent.join(format!("{prefix}{index:0width$}{suffix}"));
        let part = numbered
            .get(&index)
            .cloned()
            .ok_or_else(|| PortableImportError::MissingArchiveVolume(expected))?;
        parts.push(part);
    }
    Ok(ResolvedArchive {
        primary: parts[0].clone(),
        parts,
        suggested_name,
        signature,
        equal_sized_volumes: true,
        nested_tar,
    })
}

fn parse_trailing_numbered_volume(name: &str) -> Option<(&str, String, usize, u32)> {
    let (archive_name, digits) = name.rsplit_once('.')?;
    if archive_name.is_empty()
        || digits.len() != 3
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let index = digits.parse().ok()?;
    (index > 0).then(|| {
        (
            archive_name,
            format!("{archive_name}."),
            digits.len(),
            index,
        )
    })
}

fn parse_part_rar_name(name: &str) -> Option<(String, String, String, usize, u32)> {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".rar") {
        return None;
    }
    let stem_end = name.len().checked_sub(4)?;
    let marker = lower[..stem_end].rfind(".part")?;
    let digits = &name[marker + 5..stem_end];
    let base = &name[..marker];
    if base.is_empty()
        || digits.is_empty()
        || digits.len() > 6
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let index = digits.parse().ok()?;
    (index > 0).then(|| {
        (
            base.to_owned(),
            name[..marker + 5].to_owned(),
            name[stem_end..].to_owned(),
            digits.len(),
            index,
        )
    })
}

fn collect_numbered_archive_siblings(
    parent: &Path,
    prefix: &str,
    suffix: &str,
    width: usize,
) -> Result<BTreeMap<u32, PathBuf>, PortableImportError> {
    let mut numbered = BTreeMap::new();
    // Probe only the exact bounded set of names that can be passed to 7-Zip.
    // Scanning an attacker-controlled directory would make setup time depend
    // on every unrelated entry in Downloads and would sit outside the archive
    // worker's inspection deadline.
    for index in 1..=MAX_ARCHIVE_PARTS.saturating_add(1) {
        let path = parent.join(format!("{prefix}{index:0width$}{suffix}"));
        let exists = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(source) if source.kind() == io::ErrorKind::NotFound => false,
            Err(source) => return Err(io_error(&path, source)),
        };
        if !exists {
            continue;
        }
        if index > MAX_ARCHIVE_PARTS {
            return Err(PortableImportError::TooManyArchiveVolumes(
                MAX_ARCHIVE_PARTS,
            ));
        }
        numbered.insert(index, path);
    }
    Ok(numbered)
}

fn resolve_legacy_rar_set(
    parent: &Path,
    selected_name: &str,
) -> Result<Option<ResolvedArchive>, PortableImportError> {
    let lower = selected_name.to_ascii_lowercase();
    let (base, selected_volume, volume_prefix) = if lower.ends_with(".rar") {
        (
            &selected_name[..selected_name.len() - 4],
            None,
            format!("{}.r", &selected_name[..selected_name.len() - 4]),
        )
    } else if lower.len() > 4
        && lower.as_bytes()[lower.len() - 4] == b'.'
        && lower.as_bytes()[lower.len() - 3] == b'r'
        && lower[lower.len() - 2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        let base = &selected_name[..selected_name.len() - 4];
        let selected_volume = lower[lower.len() - 2..].parse::<u32>().ok();
        (
            base,
            selected_volume,
            selected_name[..selected_name.len() - 2].to_owned(),
        )
    } else {
        return Ok(None);
    };
    if base.is_empty() {
        return Ok(None);
    }
    let first_volume = find_case_variant(parent, &[format!("{base}.r00"), format!("{base}.R00")])?;
    if first_volume.is_none() && selected_volume.is_none() {
        return Ok(None);
    }
    let volume_prefix = if selected_volume.is_some() {
        volume_prefix
    } else {
        let first_name = first_volume
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(OsStr::to_str)
            .ok_or_else(|| PortableImportError::NonUtf8ArchiveName(parent.to_path_buf()))?;
        first_name[..first_name.len() - 2].to_owned()
    };
    let primary = find_case_variant(parent, &[format!("{base}.rar"), format!("{base}.RAR")])?
        .ok_or_else(|| {
            PortableImportError::MissingArchiveVolume(parent.join(format!("{base}.rar")))
        })?;
    let numbered = collect_zero_based_archive_siblings(parent, &volume_prefix, "", 2)?;
    let selected_volume = selected_volume.unwrap_or(0);
    let maximum = numbered
        .keys()
        .copied()
        .max()
        .unwrap_or(selected_volume)
        .max(selected_volume);
    if maximum.saturating_add(2) > MAX_ARCHIVE_PARTS {
        return Err(PortableImportError::TooManyArchiveVolumes(
            MAX_ARCHIVE_PARTS,
        ));
    }
    let mut parts = Vec::with_capacity(maximum as usize + 2);
    parts.push(primary.clone());
    for index in 0..=maximum {
        let expected = parent.join(format!("{volume_prefix}{index:02}"));
        let part = numbered
            .get(&index)
            .cloned()
            .ok_or_else(|| PortableImportError::MissingArchiveVolume(expected))?;
        parts.push(part);
    }
    Ok(Some(ResolvedArchive {
        primary,
        parts,
        suggested_name: base.to_owned(),
        signature: Some(ArchiveSignature::Rar),
        equal_sized_volumes: true,
        nested_tar: false,
    }))
}

fn resolve_classic_zip_set(
    parent: &Path,
    selected_name: &str,
) -> Result<Option<ResolvedArchive>, PortableImportError> {
    let lower = selected_name.to_ascii_lowercase();
    let (base, selected_volume, volume_prefix) = if lower.ends_with(".zip") {
        (
            &selected_name[..selected_name.len() - 4],
            None,
            format!("{}.z", &selected_name[..selected_name.len() - 4]),
        )
    } else if lower.len() > 4
        && lower.as_bytes()[lower.len() - 4] == b'.'
        && lower.as_bytes()[lower.len() - 3] == b'z'
        && lower[lower.len() - 2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        let base = &selected_name[..selected_name.len() - 4];
        let selected_volume = lower[lower.len() - 2..].parse::<u32>().ok();
        (
            base,
            selected_volume,
            selected_name[..selected_name.len() - 2].to_owned(),
        )
    } else {
        return Ok(None);
    };
    if base.is_empty() {
        return Ok(None);
    }
    let first_volume = find_case_variant(parent, &[format!("{base}.z01"), format!("{base}.Z01")])?;
    if first_volume.is_none() && selected_volume.is_none() {
        return Ok(None);
    }
    let volume_prefix = if selected_volume.is_some() {
        volume_prefix
    } else {
        let first_name = first_volume
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(OsStr::to_str)
            .ok_or_else(|| PortableImportError::NonUtf8ArchiveName(parent.to_path_buf()))?;
        first_name[..first_name.len() - 2].to_owned()
    };
    let primary = find_case_variant(parent, &[format!("{base}.zip"), format!("{base}.ZIP")])?
        .ok_or_else(|| {
            PortableImportError::MissingArchiveVolume(parent.join(format!("{base}.zip")))
        })?;
    let numbered = collect_numbered_archive_siblings(parent, &volume_prefix, "", 2)?;
    let selected_volume = selected_volume.unwrap_or(1);
    let maximum = numbered
        .keys()
        .copied()
        .max()
        .unwrap_or(selected_volume)
        .max(selected_volume);
    if maximum.saturating_add(1) > MAX_ARCHIVE_PARTS {
        return Err(PortableImportError::TooManyArchiveVolumes(
            MAX_ARCHIVE_PARTS,
        ));
    }
    let mut parts = Vec::with_capacity(maximum as usize + 1);
    parts.push(primary.clone());
    for index in 1..=maximum {
        let expected = parent.join(format!("{volume_prefix}{index:02}"));
        let part = numbered
            .get(&index)
            .cloned()
            .ok_or_else(|| PortableImportError::MissingArchiveVolume(expected))?;
        parts.push(part);
    }
    Ok(Some(ResolvedArchive {
        primary,
        parts,
        suggested_name: base.to_owned(),
        signature: None,
        equal_sized_volumes: false,
        nested_tar: false,
    }))
}

fn collect_zero_based_archive_siblings(
    parent: &Path,
    prefix: &str,
    suffix: &str,
    width: usize,
) -> Result<BTreeMap<u32, PathBuf>, PortableImportError> {
    let mut numbered = BTreeMap::new();
    for index in 0..=MAX_ARCHIVE_PARTS {
        let path = parent.join(format!("{prefix}{index:0width$}{suffix}"));
        match fs::symlink_metadata(&path) {
            Ok(_) if index < MAX_ARCHIVE_PARTS => {
                numbered.insert(index, path);
            }
            Ok(_) => {
                return Err(PortableImportError::TooManyArchiveVolumes(
                    MAX_ARCHIVE_PARTS,
                ));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Ok(numbered)
}

fn find_case_variant(
    parent: &Path,
    names: &[String],
) -> Result<Option<PathBuf>, PortableImportError> {
    let mut found = None;
    for name in names {
        let path = parent.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) if found.is_none() => found = Some(path),
            Ok(_) => {
                return Err(PortableImportError::AmbiguousArchiveVolumes(
                    parent.to_path_buf(),
                ));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Ok(found)
}

fn archive_signature_for_name(name: &str) -> Option<ArchiveSignature> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".7z") {
        Some(ArchiveSignature::SevenZip)
    } else if lower.ends_with(".rar") {
        Some(ArchiveSignature::Rar)
    } else {
        None
    }
}

fn is_compressed_tar_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".tar.gz",
        ".tgz",
        ".tar.bz2",
        ".tbz",
        ".tbz2",
        ".tar.xz",
        ".txz",
        ".tar.zst",
        ".tzst",
        ".tar.lzma",
        ".tlz",
        ".tar.z",
        ".taz",
    ]
    .iter()
    .any(|suffix| lower.ends_with(suffix))
}

fn suggested_archive_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lzma",
        ".zipx",
        ".tbz2",
        ".tgz",
        ".txz",
        ".tzst",
        ".7z",
        ".zip",
        ".rar",
        ".cab",
        ".arj",
        ".lzh",
        ".lha",
        ".tar",
        ".wim",
        ".swm",
        ".iso",
        ".xar",
        ".cpio",
        ".gz",
        ".bz2",
        ".xz",
        ".zst",
        ".lzma",
    ] {
        if lower.ends_with(suffix) && name.len() > suffix.len() {
            return name[..name.len() - suffix.len()].to_owned();
        }
    }
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or_else(|| name.to_owned(), |(stem, _)| stem.to_owned())
}

fn inspect_external_archive(
    selected: &Path,
    limits: &ImportLimits,
    destination: Option<&Path>,
    archive_password: Option<&ArchivePassword>,
) -> Result<PortableInspection, PortableImportError> {
    let inspection_deadline = Instant::now()
        .checked_add(Duration::from_secs(ARCHIVE_INSPECTION_SECONDS))
        .expect("the archive inspection deadline fits in Instant");
    let resolved = resolve_archive_parts(selected)?;
    let opened = open_archive_parts(&resolved, limits)?;
    let before = opened.identities.clone();
    let (mut listed, probe_paths) = list_external_archive(
        &resolved,
        &opened,
        limits,
        inspection_deadline,
        archive_password,
    )?;
    for relative in probe_paths {
        if let Some(runner) = probe_archive_launcher(
            &resolved,
            &opened,
            &relative,
            limits,
            inspection_deadline,
            archive_password,
        )? {
            listed.add_candidate(Path::new(PORTABLE_GAME_ROOT).join(relative), runner, limits)?;
        }
    }
    listed.candidates.sort();

    let inspection = if let Some(destination) = destination {
        extract_external_archive(&resolved, &opened, destination, limits, archive_password)?;
        // Windows-oriented archive formats such as RAR commonly carry no Unix
        // mode bits even when their payload also contains a native Linux
        // build. Restore execute permission from file magic before the image
        // is finalized so shell launchers can execute extensionless runtimes
        // such as Ren'Py's lib/linux-x86_64/<game> binary.
        let mut extracted = scan_directory(destination, limits, None, true)?;
        let compressed_bytes = archive_identity_total_size(&before)?;
        if compressed_bytes == 0
            || extracted.bytes > compressed_bytes.saturating_mul(limits.max_compression_ratio)
        {
            return Err(PortableImportError::ArchiveCompressionRatioExceeded);
        }
        if extracted.files != listed.files {
            return Err(PortableImportError::ArchiveExtractionMismatch);
        }
        extracted.suggested_name = resolved.suggested_name.clone();
        extracted.candidates.sort();
        if extracted.candidates != listed.candidates {
            return Err(PortableImportError::ArchiveExtractionMismatch);
        }
        extracted.finish()
    } else {
        listed.finish()
    };

    let after = snapshot_opened_archive(&opened)?;
    if before != after {
        return Err(PortableImportError::SourceChanged(selected.to_path_buf()));
    }
    Ok(PortableInspection {
        suggested_name: resolved.suggested_name,
        ..inspection
    })
}

struct OpenedArchive {
    files: Vec<File>,
    identities: Vec<ArchivePartIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchivePartIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: u64,
    changed_seconds: i64,
    changed_nanoseconds: u64,
}

fn snapshot_opened_archive(
    opened: &OpenedArchive,
) -> Result<Vec<ArchivePartIdentity>, PortableImportError> {
    if opened.files.len() != opened.identities.len() {
        return Err(PortableImportError::ArchiveExtractionMismatch);
    }
    opened
        .files
        .iter()
        .zip(&opened.identities)
        .map(|(file, original)| {
            let stat = fstat(file).map_err(|error| io_error(&original.path, error.into()))?;
            Ok(archive_part_identity(original.path.clone(), &stat))
        })
        .collect()
}

fn archive_part_identity(path: PathBuf, stat: &Stat) -> ArchivePartIdentity {
    ArchivePartIdentity {
        path,
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode,
        links: stat.st_nlink as u64,
        size: stat.st_size as u64,
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
        changed_seconds: stat.st_ctime,
        changed_nanoseconds: stat.st_ctime_nsec,
    }
}

fn archive_identity_total_size(
    identities: &[ArchivePartIdentity],
) -> Result<u64, PortableImportError> {
    identities.iter().try_fold(0_u64, |total, part| {
        total
            .checked_add(part.size)
            .ok_or(PortableImportError::SizeOverflow)
    })
}

fn open_archive_parts(
    resolved: &ResolvedArchive,
    limits: &ImportLimits,
) -> Result<OpenedArchive, PortableImportError> {
    let mut identities = Vec::with_capacity(resolved.parts.len());
    let mut files = Vec::with_capacity(resolved.parts.len());
    let mut total = 0_u64;
    for path in &resolved.parts {
        let fd = openat2(
            CWD,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
            ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|error| io_error(path, error.into()))?;
        let stat = fstat(&fd).map_err(|error| io_error(path, error.into()))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(PortableImportError::SourceNotRegularFile(path.clone()));
        }
        if stat.st_nlink != 1 {
            return Err(PortableImportError::HardLink(path.clone()));
        }
        if stat.st_size < 0 {
            return Err(PortableImportError::InvalidFileSize(path.clone()));
        }
        let size = stat.st_size as u64;
        if size == 0 {
            if matching_partial_download_exists(path) {
                return Err(PortableImportError::DownloadIncomplete(path.clone()));
            }
            return Err(PortableImportError::InvalidArchiveVolumeSize {
                path: path.clone(),
                bytes: 0,
            });
        }
        total = total
            .checked_add(size)
            .ok_or(PortableImportError::SizeOverflow)?;
        if total > limits.max_zip_bytes {
            return Err(PortableImportError::ArchiveTooLarge {
                bytes: total,
                limit: limits.max_zip_bytes,
            });
        }
        let file = File::from(fd);
        if path == &resolved.primary
            && let Some(expected) = resolved.signature
        {
            let expected = expected.bytes();
            let mut signature = vec![0_u8; expected.len()];
            file.read_exact_at(&mut signature, 0)
                .map_err(|source| io_error(path, source))?;
            if signature != expected {
                return Err(PortableImportError::InvalidArchiveSignature(path.clone()));
            }
        }
        identities.push(archive_part_identity(path.clone(), &stat));
        files.push(file);
    }
    if identities.len() > 1 && resolved.equal_sized_volumes {
        let expected = identities[0].size;
        for part in &identities[..identities.len() - 1] {
            if part.size != expected {
                return Err(PortableImportError::InvalidArchiveVolumeSize {
                    path: part.path.clone(),
                    bytes: part.size,
                });
            }
        }
        let final_part = identities
            .last()
            .expect("multi-part archive has a final part");
        if final_part.size > expected {
            return Err(PortableImportError::InvalidArchiveVolumeSize {
                path: final_part.path.clone(),
                bytes: final_part.size,
            });
        }
    }
    Ok(OpenedArchive { files, identities })
}

/// Firefox and Tor Browser keep a zero-byte final name beside an active
/// download such as `Game.a1B2c3D4.zip.part`. For names containing dots, the
/// random token can appear before an earlier suffix. Recognize only the exact
/// target obtained by removing one eight-character browser token, or the
/// conventional direct `.part` suffix.
fn matching_partial_download_exists(target: &Path) -> bool {
    let (Some(parent), Some(target_name)) = (target.parent(), target.file_name()) else {
        return false;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        metadata.file_type().is_file()
            && partial_download_name_matches(target_name, &entry.file_name())
    })
}

fn partial_download_name_matches(target: &OsStr, candidate: &OsStr) -> bool {
    let target = target.as_bytes();
    let candidate = candidate.as_bytes();
    let Some(partial) = candidate.strip_suffix(b".part") else {
        return false;
    };
    if partial == target {
        return true;
    }
    for token_start in 1..partial.len() {
        if partial[token_start - 1] != b'.' {
            continue;
        }
        let Some(relative_end) = partial[token_start..].iter().position(|byte| *byte == b'.')
        else {
            continue;
        };
        let token_end = token_start + relative_end;
        let token = &partial[token_start..token_end];
        if token.len() != 8 || !token.iter().all(u8::is_ascii_alphanumeric) {
            continue;
        }
        let mut without_token = Vec::with_capacity(partial.len() - token.len() - 1);
        without_token.extend_from_slice(&partial[..token_start - 1]);
        without_token.extend_from_slice(&partial[token_end..]);
        if without_token == target {
            return true;
        }
    }
    false
}

fn list_external_archive(
    resolved: &ResolvedArchive,
    opened: &OpenedArchive,
    limits: &ImportLimits,
    deadline: Instant,
    archive_password: Option<&ArchivePassword>,
) -> Result<(ScanState, Vec<PathBuf>), PortableImportError> {
    let primary = sandbox_primary_path(resolved)?;
    let timeout = remaining_inspection_time(deadline)?;
    let stdout = if resolved.nested_tar {
        let producer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            outer_stream_arguments(primary),
        )?;
        let consumer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            tar_stream_listing_arguments(),
        )?;
        run_archive_pipeline(
            producer,
            consumer,
            ARCHIVE_LISTING_LIMIT,
            false,
            archive_password,
        )?
    } else {
        let command = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            [
                OsString::from("l"),
                OsString::from("-slt"),
                OsString::from("-ba"),
                OsString::from("-bd"),
                OsString::from("-bb0"),
                OsString::from("-mmt2"),
                OsString::from("-sccUTF-8"),
                OsString::from("--"),
                primary.into_os_string(),
            ],
        )?;
        run_archive_command(command, ARCHIVE_LISTING_LIMIT, archive_password)?
    };
    parse_external_listing(
        &stdout,
        resolved,
        archive_identity_total_size(&opened.identities)?,
        limits,
        archive_password.is_some(),
    )
}

fn parse_external_listing(
    bytes: &[u8],
    resolved: &ResolvedArchive,
    compressed_bytes: u64,
    limits: &ImportLimits,
    allow_encrypted: bool,
) -> Result<(ScanState, Vec<PathBuf>), PortableImportError> {
    let listing =
        std::str::from_utf8(bytes).map_err(|_| PortableImportError::NonUtf8ArchiveListing)?;
    let listing = strip_archive_password_prompt(listing, allow_encrypted);
    let mut state = ScanState::new(resolved.suggested_name.clone());
    let mut probes = Vec::new();
    for block in listing.split("\n\n") {
        let block = block.trim_matches(['\r', '\n']);
        if block.is_empty() {
            continue;
        }
        let mut path = None;
        let mut size = None;
        let mut attributes = None;
        let mut mode = None;
        let mut encrypted = None;
        let mut folder = None;
        let mut unsafe_link = false;
        let mut anti_item = false;
        for raw_line in block.lines() {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            let (key, value) = line
                .split_once(" = ")
                .ok_or_else(|| PortableImportError::MalformedArchiveListing(line.into()))?;
            match key {
                "Path" => set_listing_field(&mut path, value.to_owned(), key)?,
                "Size" => {
                    let parsed = value
                        .parse::<u64>()
                        .map_err(|_| PortableImportError::MalformedArchiveListing(line.into()))?;
                    set_listing_field(&mut size, parsed, key)?;
                }
                "Attributes" => set_listing_field(&mut attributes, value.to_owned(), key)?,
                "Mode" => set_listing_field(&mut mode, value.to_owned(), key)?,
                "Encrypted" => set_listing_field(&mut encrypted, value.to_owned(), key)?,
                "Folder" => set_listing_field(&mut folder, value.to_owned(), key)?,
                "Symbolic Link" | "Hard Link" => unsafe_link |= !value.is_empty(),
                "Anti" => anti_item |= value == "+",
                _ => {}
            }
        }
        let raw_path = path.ok_or_else(|| {
            PortableImportError::MalformedArchiveListing("entry has no path".into())
        })?;
        if raw_path.contains('\\') {
            return Err(PortableImportError::UnsafeWindowsPath(raw_path));
        }
        if !allow_encrypted
            && encrypted
                .as_deref()
                .is_some_and(|value| !value.is_empty() && value != "-")
        {
            return Err(PortableImportError::EncryptedArchive);
        }
        if unsafe_link
            || anti_item
            || attributes
                .as_deref()
                .is_some_and(archive_attributes_are_unsafe)
            || mode.as_deref().is_some_and(archive_mode_is_unsafe)
        {
            return Err(PortableImportError::UnsupportedArchiveFileType(
                PathBuf::from(raw_path),
            ));
        }
        let is_directory = folder.as_deref() == Some("+")
            || attributes
                .as_deref()
                .is_some_and(archive_attributes_are_directory)
            || mode.as_deref().is_some_and(|mode| mode.starts_with('d'));
        let trimmed = if is_directory {
            raw_path.trim_end_matches('/')
        } else {
            raw_path.as_str()
        };
        if trimmed.is_empty() || trimmed.split('/').any(str::is_empty) {
            return Err(PortableImportError::UnsafeWindowsPath(raw_path));
        }
        let relative = PathBuf::from(trimmed);
        let size = size.ok_or_else(|| {
            PortableImportError::MalformedArchiveListing("entry has no size".into())
        })?;
        if is_directory && size != 0 {
            return Err(PortableImportError::MalformedArchiveListing(format!(
                "directory has non-zero size: {trimmed}"
            )));
        }
        state.add_entry(
            &relative,
            is_directory,
            if is_directory { 0 } else { size },
            limits,
        )?;
        if !is_directory && has_launcher_extension(&relative) {
            if probes.len() as u64 >= limits.max_executable_candidates {
                return Err(PortableImportError::TooManyExecutableCandidates(
                    limits.max_executable_candidates,
                ));
            }
            probes.push(relative);
        }
    }
    validate_manifest_prefix_conflicts(&state.windows_paths)?;
    if compressed_bytes == 0 && state.bytes > 0
        || compressed_bytes > 0
            && state.bytes > compressed_bytes.saturating_mul(limits.max_compression_ratio)
    {
        return Err(PortableImportError::ArchiveCompressionRatioExceeded);
    }
    Ok((state, probes))
}

fn strip_archive_password_prompt(listing: &str, password_supplied: bool) -> &str {
    if !password_supplied {
        return listing;
    }
    let without_leading_breaks = listing.trim_start_matches(['\r', '\n']);
    let Some((first_line, remaining)) = without_leading_breaks.split_once('\n') else {
        return listing;
    };
    match first_line.trim_end_matches('\r') {
        "Enter password:" | "Enter password (will not be echoed):" => remaining,
        _ => listing,
    }
}

fn set_listing_field<T>(
    target: &mut Option<T>,
    value: T,
    name: &str,
) -> Result<(), PortableImportError> {
    if target.replace(value).is_some() {
        return Err(PortableImportError::MalformedArchiveListing(format!(
            "duplicate {name} field"
        )));
    }
    Ok(())
}

fn archive_attributes_are_directory(attributes: &str) -> bool {
    attributes
        .split_whitespace()
        .next()
        .is_some_and(|dos| dos.contains('D'))
        || attributes
            .split_whitespace()
            .nth(1)
            .is_some_and(|mode| mode.starts_with('d'))
}

fn archive_attributes_are_unsafe(attributes: &str) -> bool {
    attributes
        .split_whitespace()
        .nth(1)
        .and_then(|mode| mode.chars().next())
        .is_some_and(|kind| !matches!(kind, '-' | 'd'))
}

fn archive_mode_is_unsafe(mode: &str) -> bool {
    mode.chars()
        .next()
        .is_some_and(|kind| !matches!(kind, '-' | 'd'))
}

fn probe_archive_launcher(
    resolved: &ResolvedArchive,
    opened: &OpenedArchive,
    relative: &Path,
    limits: &ImportLimits,
    deadline: Instant,
    archive_password: Option<&ArchivePassword>,
) -> Result<Option<RunnerKind>, PortableImportError> {
    let primary = sandbox_primary_path(resolved)?;
    if resolved.nested_tar {
        let timeout = remaining_inspection_time(deadline)?;
        let producer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            outer_stream_arguments(primary),
        )?;
        let consumer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            tar_stream_probe_arguments(relative),
        )?;
        let prefix = run_archive_pipeline(producer, consumer, 4, true, archive_password)?;
        return Ok(classify_launcher(relative, &prefix));
    }
    let mut command = archive_command(
        resolved,
        opened,
        remaining_inspection_time(deadline)?,
        limits.max_file_bytes,
        None,
        [
            OsString::from("x"),
            OsString::from("-so"),
            OsString::from("-bd"),
            OsString::from("-bb0"),
            OsString::from("-mmt2"),
            OsString::from("-bse0"),
            OsString::from("-bsp0"),
            OsString::from("-spd"),
            OsString::from("--"),
            primary.into_os_string(),
            relative.as_os_str().to_owned(),
        ],
    )?;
    if archive_password.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| PortableImportError::ArchiveToolIo {
            operation: "probe",
            source,
        })?;
    write_archive_password(&mut child, archive_password);
    let mut prefix = [0_u8; 4];
    let mut read = 0;
    if let Some(mut stdout) = child.stdout.take() {
        while read < prefix.len() {
            match stdout.read(&mut prefix[read..]) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PortableImportError::ArchiveToolIo {
                        operation: "probe output",
                        source: error,
                    });
                }
            }
        }
        drop(stdout);
    }
    let status = match child.wait() {
        Ok(status) => status,
        Err(source) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PortableImportError::ArchiveToolIo {
                operation: "probe wait",
                source,
            });
        }
    };
    if read < 2 && !status.success() {
        return Err(PortableImportError::ArchiveToolFailed {
            status: status.code(),
            diagnostic: "could not read an executable candidate".into(),
        });
    }
    Ok(classify_launcher(relative, &prefix[..read]))
}

fn extract_external_archive(
    resolved: &ResolvedArchive,
    opened: &OpenedArchive,
    destination: &Path,
    limits: &ImportLimits,
    archive_password: Option<&ArchivePassword>,
) -> Result<(), PortableImportError> {
    if fs::read_dir(destination)
        .map_err(|source| io_error(destination, source))?
        .next()
        .is_some()
    {
        return Err(PortableImportError::ArchiveDestinationNotEmpty(
            destination.to_path_buf(),
        ));
    }
    let primary = sandbox_primary_path(resolved)?;
    let timeout = Duration::from_secs(ARCHIVE_EXTRACTION_SECONDS);
    if resolved.nested_tar {
        let producer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            None,
            outer_stream_arguments(primary),
        )?;
        let consumer = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            Some(destination),
            tar_stream_extraction_arguments(),
        )?;
        let _ = run_archive_pipeline(
            producer,
            consumer,
            ARCHIVE_DIAGNOSTIC_LIMIT,
            false,
            archive_password,
        )?;
    } else {
        let command = archive_command(
            resolved,
            opened,
            timeout,
            limits.max_file_bytes,
            Some(destination),
            external_extraction_arguments(primary),
        )?;
        let _ = run_archive_command(command, ARCHIVE_DIAGNOSTIC_LIMIT, archive_password)?;
    }
    Ok(())
}

fn outer_stream_arguments(primary: PathBuf) -> Vec<OsString> {
    vec![
        OsString::from("x"),
        OsString::from("-so"),
        OsString::from("-bd"),
        OsString::from("-bb0"),
        OsString::from("-mmt2"),
        OsString::from("-bse0"),
        OsString::from("-bsp0"),
        OsString::from("--"),
        primary.into_os_string(),
    ]
}

fn tar_stream_listing_arguments() -> Vec<OsString> {
    vec![
        OsString::from("l"),
        OsString::from("-slt"),
        OsString::from("-ba"),
        OsString::from("-bd"),
        OsString::from("-bb0"),
        OsString::from("-mmt2"),
        OsString::from("-sccUTF-8"),
        OsString::from("-ttar"),
        OsString::from("-si"),
    ]
}

fn tar_stream_probe_arguments(relative: &Path) -> Vec<OsString> {
    vec![
        OsString::from("x"),
        OsString::from("-so"),
        OsString::from("-bd"),
        OsString::from("-bb0"),
        OsString::from("-mmt2"),
        OsString::from("-bse0"),
        OsString::from("-bsp0"),
        OsString::from("-ttar"),
        OsString::from("-si"),
        OsString::from("--"),
        relative.as_os_str().to_owned(),
    ]
}

fn tar_stream_extraction_arguments() -> Vec<OsString> {
    vec![
        OsString::from("x"),
        OsString::from("-bd"),
        OsString::from("-bb0"),
        OsString::from("-mmt2"),
        OsString::from("-bso0"),
        OsString::from("-bsp0"),
        OsString::from("-sccUTF-8"),
        OsString::from("-ttar"),
        OsString::from("-si"),
        OsString::from("-o/output"),
    ]
}

fn external_extraction_arguments(primary: PathBuf) -> Vec<OsString> {
    vec![
        OsString::from("x"),
        OsString::from("-bd"),
        OsString::from("-bb0"),
        OsString::from("-mmt2"),
        OsString::from("-bso0"),
        OsString::from("-bsp0"),
        OsString::from("-sccUTF-8"),
        // Default `x` mode preserves relative directories. There is no
        // portable `-spf off` switch; every `-spf` variant changes path mode.
        OsString::from("-o/output"),
        OsString::from("--"),
        primary.into_os_string(),
    ]
}

fn sandbox_primary_path(resolved: &ResolvedArchive) -> Result<PathBuf, PortableImportError> {
    let name = resolved
        .primary
        .file_name()
        .ok_or_else(|| PortableImportError::UnsupportedArchiveName(resolved.primary.clone()))?;
    Ok(Path::new("/input").join(name))
}

fn archive_command<I>(
    resolved: &ResolvedArchive,
    opened: &OpenedArchive,
    timeout: Duration,
    max_file_bytes: u64,
    destination: Option<&Path>,
    seven_zip_arguments: I,
) -> Result<Command, PortableImportError>
where
    I: IntoIterator<Item = OsString>,
{
    let bwrap = runtime_tool("CAPSULE_BUBBLEWRAP", BWRAP_PATH)?;
    let timeout_tool = runtime_tool("CAPSULE_TIMEOUT", TIMEOUT_PATH)?;
    let prlimit = runtime_tool("CAPSULE_PRLIMIT", PRLIMIT_PATH)?;
    let seven_zip = runtime_tool("CAPSULE_7Z", SEVEN_ZIP_PATH)?;
    let seven_zip_plugin = runtime_file("CAPSULE_7Z_PLUGIN", SEVEN_ZIP_PLUGIN_PATH)?;

    let timeout_millis = timeout.as_millis().clamp(1, u64::MAX as u128) as u64;
    let timeout_argument = format!("{}.{:03}s", timeout_millis / 1_000, timeout_millis % 1_000);
    let cpu_seconds = timeout
        .as_secs()
        .saturating_add(u64::from(timeout.subsec_nanos() != 0))
        .max(1);

    let mut command = Command::new(&timeout_tool);
    command
        .arg("--signal=TERM")
        .arg("--kill-after=5s")
        .arg(timeout_argument)
        .arg(&bwrap)
        .arg("--unshare-user")
        .arg("--unshare-pid")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--unshare-net")
        .arg("--disable-userns")
        .arg("--assert-userns-disabled")
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--cap-drop")
        .arg("ALL")
        .arg("--dir")
        .arg("/usr")
        .arg("--dir")
        .arg("/usr/bin")
        .arg("--dir")
        .arg("/usr/lib")
        .arg("--dir")
        .arg("/usr/lib/7zip")
        .arg("--dir")
        .arg("/lib64")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--size")
        .arg(ARCHIVE_TMPFS_LIMIT.to_string())
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--dir")
        .arg("/input")
        .arg("--clearenv")
        .arg("--setenv")
        .arg("HOME")
        .arg("/tmp")
        .arg("--setenv")
        .arg("TMPDIR")
        .arg("/tmp")
        .arg("--setenv")
        .arg("LC_ALL")
        .arg("C.UTF-8");

    for (source, target) in [
        (prlimit.as_path(), PRLIMIT_PATH),
        (seven_zip.as_path(), SEVEN_ZIP_PATH),
        (seven_zip_plugin.as_path(), SEVEN_ZIP_PLUGIN_PATH),
    ] {
        command.arg("--ro-bind").arg(source).arg(target);
    }
    for (default_source, target) in SEVEN_ZIP_RUNTIME_FILES {
        let source = bundled_runtime_file(default_source);
        require_trusted_runtime_file(&source)?;
        command.arg("--ro-bind").arg(source).arg(target);
    }

    if opened.files.len() != resolved.parts.len() {
        return Err(PortableImportError::ArchiveExtractionMismatch);
    }
    let mut inherited_parts = Vec::with_capacity(resolved.parts.len());
    for (part, held_file) in resolved.parts.iter().zip(&opened.files) {
        let name = part
            .file_name()
            .ok_or_else(|| PortableImportError::UnsupportedArchiveName(part.clone()))?;
        let fd = fcntl_dupfd_cloexec(held_file, 3).map_err(|source| {
            PortableImportError::ArchiveToolIo {
                operation: "duplicate source descriptor",
                source: source.into(),
            }
        })?;
        command
            .arg("--ro-bind-fd")
            .arg(fd.as_raw_fd().to_string())
            .arg(Path::new("/input").join(name));
        inherited_parts.push(fd);
    }
    if let Some(destination) = destination {
        command
            .arg("--dir")
            .arg("/output")
            .arg("--bind")
            .arg(destination)
            .arg("/output")
            .arg("--chdir")
            .arg("/output");
    } else {
        command.arg("--chdir").arg("/tmp");
    }
    // Apply per-process limits only after Bubblewrap creates the private user
    // and PID namespaces. RLIMIT_NPROC is charged to a real user identity; if
    // it wraps Bubblewrap itself, the user's existing desktop threads can
    // exhaust a small limit before the namespace is created. Inside this
    // namespace the limit covers the untrusted parser and its descendants.
    command
        .arg("--")
        .arg(PRLIMIT_PATH)
        .arg(format!("--as={ARCHIVE_MEMORY_LIMIT}"))
        .arg(format!("--nproc={ARCHIVE_PROCESS_LIMIT}"))
        .arg("--nofile=256")
        .arg(format!("--cpu={cpu_seconds}"))
        .arg(format!("--fsize={max_file_bytes}"))
        .arg("--")
        .arg(SEVEN_ZIP_PATH)
        .args(seven_zip_arguments);
    command.env_clear();
    // `Command` keeps the O_CLOEXEC descriptors alive. In the post-fork child
    // only, clear CLOEXEC so timeout -> bwrap can pass the already
    // opened read-only files to `--ro-bind-fd`. No pathname is reopened after
    // validation and no source directory is visible inside the archive parser.
    unsafe {
        command.pre_exec(move || {
            for fd in &inherited_parts {
                fcntl_setfd(fd, FdFlags::empty()).map_err(io::Error::from)?;
            }
            Ok(())
        });
    }
    Ok(command)
}

fn remaining_inspection_time(deadline: Instant) -> Result<Duration, PortableImportError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(PortableImportError::ArchiveInspectionTimedOut)
}

fn trusted_tool_available(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_file() && metadata.permissions().mode() & 0o111 != 0
        })
        .unwrap_or(false)
}

fn runtime_tool(variable: &str, default: &str) -> Result<PathBuf, PortableImportError> {
    let path = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default));
    require_trusted_tool(&path)?;
    Ok(path)
}

fn runtime_file(variable: &str, default: &str) -> Result<PathBuf, PortableImportError> {
    let path = std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| bundled_runtime_file(default));
    require_trusted_runtime_file(&path)?;
    Ok(path)
}

fn bundled_runtime_file(default: &str) -> PathBuf {
    let Some(root) = std::env::var_os("CAPSULE_BUNDLE_ROOT").filter(|value| !value.is_empty())
    else {
        return PathBuf::from(default);
    };
    let relative = Path::new(default)
        .strip_prefix("/")
        .expect("runtime file defaults are absolute");
    let bundled = PathBuf::from(root).join(relative);
    if bundled.is_file() {
        bundled
    } else {
        PathBuf::from(default)
    }
}

fn require_trusted_tool(path: &Path) -> Result<(), PortableImportError> {
    if path.is_absolute() && trusted_tool_available(path) {
        Ok(())
    } else {
        Err(PortableImportError::ArchiveToolUnavailable(
            path.to_path_buf(),
        ))
    }
}

fn require_trusted_runtime_file(path: &Path) -> Result<(), PortableImportError> {
    if path.is_absolute() && fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        Ok(())
    } else {
        Err(PortableImportError::ArchiveToolUnavailable(
            path.to_path_buf(),
        ))
    }
}

fn run_archive_command(
    mut command: Command,
    stdout_limit: u64,
    archive_password: Option<&ArchivePassword>,
) -> Result<Vec<u8>, PortableImportError> {
    if archive_password.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| PortableImportError::ArchiveToolIo {
            operation: "start",
            source,
        })?;
    write_archive_password(&mut child, archive_password);
    let stdout = child.stdout.take().expect("piped archive stdout");
    let stderr = child.stderr.take().expect("piped archive stderr");
    let stdout_reader = thread::spawn(move || read_bounded_output(stdout, stdout_limit));
    let stderr_reader =
        thread::spawn(move || read_bounded_output(stderr, ARCHIVE_DIAGNOSTIC_LIMIT));
    let status = match child.wait() {
        Ok(status) => status,
        Err(source) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(PortableImportError::ArchiveToolIo {
                operation: "wait",
                source,
            });
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| PortableImportError::ArchiveOutputWorkerFailed)??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| PortableImportError::ArchiveOutputWorkerFailed)??;
    if stdout.overflow {
        return Err(PortableImportError::ArchiveListingTooLarge(stdout_limit));
    }
    if stderr.overflow {
        return Err(PortableImportError::ArchiveDiagnosticTooLarge(
            ARCHIVE_DIAGNOSTIC_LIMIT,
        ));
    }
    if !status.success() {
        let diagnostic = sanitize_diagnostic(&stderr.bytes);
        if archive_output_mentions_password(&stdout.bytes, &stderr.bytes) {
            return Err(PortableImportError::EncryptedArchive);
        }
        return Err(PortableImportError::ArchiveToolFailed {
            status: status.code(),
            diagnostic,
        });
    }
    Ok(stdout.bytes)
}

fn run_archive_pipeline(
    mut producer_command: Command,
    mut consumer_command: Command,
    stdout_limit: u64,
    allow_stdout_overflow: bool,
    archive_password: Option<&ArchivePassword>,
) -> Result<Vec<u8>, PortableImportError> {
    if archive_password.is_some() {
        producer_command.stdin(Stdio::piped());
    } else {
        producer_command.stdin(Stdio::null());
    }
    let mut producer = producer_command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| PortableImportError::ArchiveToolIo {
            operation: "start compressed archive reader",
            source,
        })?;
    write_archive_password(&mut producer, archive_password);
    let producer_stdout = producer
        .stdout
        .take()
        .expect("piped compressed archive output");
    let producer_stderr = producer
        .stderr
        .take()
        .expect("piped compressed archive diagnostics");
    let mut consumer = match consumer_command
        .stdin(Stdio::from(producer_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(consumer) => consumer,
        Err(source) => {
            let _ = producer.kill();
            let _ = producer.wait();
            return Err(PortableImportError::ArchiveToolIo {
                operation: "start inner archive reader",
                source,
            });
        }
    };
    let consumer_stdout = consumer.stdout.take().expect("piped archive output");
    let consumer_stderr = consumer.stderr.take().expect("piped archive diagnostics");
    let producer_stderr_reader =
        thread::spawn(move || read_bounded_output(producer_stderr, ARCHIVE_DIAGNOSTIC_LIMIT));
    let consumer_stdout_reader =
        thread::spawn(move || read_bounded_output(consumer_stdout, stdout_limit));
    let consumer_stderr_reader =
        thread::spawn(move || read_bounded_output(consumer_stderr, ARCHIVE_DIAGNOSTIC_LIMIT));

    let consumer_status = match consumer.wait() {
        Ok(status) => status,
        Err(source) => {
            let _ = consumer.kill();
            let _ = producer.kill();
            let _ = consumer.wait();
            let _ = producer.wait();
            let _ = producer_stderr_reader.join();
            let _ = consumer_stdout_reader.join();
            let _ = consumer_stderr_reader.join();
            return Err(PortableImportError::ArchiveToolIo {
                operation: "wait for inner archive reader",
                source,
            });
        }
    };
    let producer_status = match producer.wait() {
        Ok(status) => status,
        Err(source) => {
            let _ = producer.kill();
            let _ = producer.wait();
            let _ = producer_stderr_reader.join();
            let _ = consumer_stdout_reader.join();
            let _ = consumer_stderr_reader.join();
            return Err(PortableImportError::ArchiveToolIo {
                operation: "wait for compressed archive reader",
                source,
            });
        }
    };
    let producer_stderr = producer_stderr_reader
        .join()
        .map_err(|_| PortableImportError::ArchiveOutputWorkerFailed)??;
    let stdout = consumer_stdout_reader
        .join()
        .map_err(|_| PortableImportError::ArchiveOutputWorkerFailed)??;
    let consumer_stderr = consumer_stderr_reader
        .join()
        .map_err(|_| PortableImportError::ArchiveOutputWorkerFailed)??;
    if stdout.overflow && !allow_stdout_overflow {
        return Err(PortableImportError::ArchiveListingTooLarge(stdout_limit));
    }
    if producer_stderr.overflow || consumer_stderr.overflow {
        return Err(PortableImportError::ArchiveDiagnosticTooLarge(
            ARCHIVE_DIAGNOSTIC_LIMIT,
        ));
    }
    if !producer_status.success() || !consumer_status.success() {
        let mut diagnostics = producer_stderr.bytes;
        if !diagnostics.is_empty() && !consumer_stderr.bytes.is_empty() {
            diagnostics.push(b'\n');
        }
        diagnostics.extend_from_slice(&consumer_stderr.bytes);
        let diagnostic = sanitize_diagnostic(&diagnostics);
        if diagnostic.to_ascii_lowercase().contains("password")
            || diagnostic.to_ascii_lowercase().contains("encrypted")
        {
            return Err(PortableImportError::EncryptedArchive);
        }
        let status = if !producer_status.success() {
            producer_status.code()
        } else {
            consumer_status.code()
        };
        return Err(PortableImportError::ArchiveToolFailed { status, diagnostic });
    }
    Ok(stdout.bytes)
}

fn write_archive_password(
    child: &mut std::process::Child,
    archive_password: Option<&ArchivePassword>,
) {
    let Some(password) = archive_password else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A pipe keeps the credential out of argv, `/proc/*/cmdline`, the
        // Sandwine/Bubblewrap diagnostics and Capsule's persistent metadata.
        let _ = stdin.write_all(password.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
}

fn archive_output_mentions_password(stdout: &[u8], stderr: &[u8]) -> bool {
    [stdout, stderr].into_iter().any(|bytes| {
        let diagnostic = sanitize_diagnostic(bytes).to_ascii_lowercase();
        diagnostic.contains("password") || diagnostic.contains("encrypted")
    })
}

struct BoundedOutput {
    bytes: Vec<u8>,
    overflow: bool,
}

fn read_bounded_output<R: Read>(
    mut reader: R,
    limit: u64,
) -> Result<BoundedOutput, PortableImportError> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read =
            reader
                .read(&mut buffer)
                .map_err(|source| PortableImportError::ArchiveToolIo {
                    operation: "read output",
                    source,
                })?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(bytes.len() as u64) as usize;
        let retained = read.min(remaining);
        bytes.extend_from_slice(&buffer[..retained]);
        overflow |= retained < read;
    }
    Ok(BoundedOutput { bytes, overflow })
}

fn sanitize_diagnostic(bytes: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(bytes);
    let cleaned: String = rendered
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "the archive may be incomplete; keep every volume in the same folder".into()
    } else {
        cleaned.to_owned()
    }
}

struct ZipManifestEntry {
    index: usize,
    relative: PathBuf,
    is_directory: bool,
    size: u64,
    mode: u32,
}

struct ZipPreflight {
    entries: u64,
}

fn preflight_zip(
    file: &File,
    source: &Path,
    limits: &ImportLimits,
) -> Result<ZipPreflight, PortableImportError> {
    let size = fstat(file)
        .map_err(|error| io_error(source, error.into()))?
        .st_size;
    if size < ZIP_EOCD_MIN as i64 {
        return Err(PortableImportError::MalformedZip(
            "missing ZIP trailer".into(),
        ));
    }
    let size = size as u64;
    let window_len = size.min(ZIP_EOCD_WINDOW) as usize;
    let mut window = vec![0_u8; window_len];
    let mut reader = file
        .try_clone()
        .map_err(|source_error| io_error(source, source_error))?;
    reader
        .seek(SeekFrom::End(-(window_len as i64)))
        .and_then(|_| reader.read_exact(&mut window))
        .map_err(|source_error| io_error(source, source_error))?;
    let position = window
        .windows(4)
        .rposition(|candidate| candidate == ZIP_EOCD_SIGNATURE)
        .ok_or_else(|| PortableImportError::MalformedZip("missing ZIP trailer".into()))?;
    if position + ZIP_EOCD_MIN > window.len() {
        return Err(PortableImportError::MalformedZip(
            "truncated ZIP trailer".into(),
        ));
    }
    let trailer = &window[position..];
    let comment_len = read_u16(trailer, 20) as usize;
    if ZIP_EOCD_MIN + comment_len != trailer.len() {
        return Err(PortableImportError::MalformedZip(
            "trailing or concatenated data is not accepted".into(),
        ));
    }
    let disk = read_u16(trailer, 4);
    let central_disk = read_u16(trailer, 6);
    let disk_entries = read_u16(trailer, 8);
    let entries = read_u16(trailer, 10);
    let central_size = read_u32(trailer, 12) as u64;
    let central_offset = read_u32(trailer, 16) as u64;
    if disk != 0 || central_disk != 0 || disk_entries != entries {
        return Err(PortableImportError::MalformedZip(
            "multi-disk ZIP archives are not accepted".into(),
        ));
    }
    if entries == u16::MAX || central_size == u32::MAX as u64 || central_offset == u32::MAX as u64 {
        return Err(PortableImportError::MalformedZip(
            "ZIP64 archives are not accepted yet".into(),
        ));
    }
    if entries as u64 > limits.max_entries {
        return Err(PortableImportError::TooManyEntries(limits.max_entries));
    }
    if central_size > limits.max_central_directory_bytes {
        return Err(PortableImportError::CentralDirectoryTooLarge {
            bytes: central_size,
            limit: limits.max_central_directory_bytes,
        });
    }
    let eocd_offset = size - window_len as u64 + position as u64;
    let central_end = central_offset
        .checked_add(central_size)
        .ok_or(PortableImportError::SizeOverflow)?;
    if central_end != eocd_offset {
        return Err(PortableImportError::MalformedZip(
            "central directory is overlapping or ambiguous".into(),
        ));
    }
    if entries > 0 {
        let mut signature = [0_u8; 4];
        reader
            .seek(SeekFrom::Start(central_offset))
            .and_then(|_| reader.read_exact(&mut signature))
            .map_err(|source_error| io_error(source, source_error))?;
        if &signature != ZIP_CENTRAL_SIGNATURE {
            return Err(PortableImportError::MalformedZip(
                "central directory signature is invalid".into(),
            ));
        }
    }
    Ok(ZipPreflight {
        entries: entries as u64,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn open_root_directory(path: &Path) -> Result<OwnedFd, PortableImportError> {
    let fd = openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|error| io_error(path, error.into()))?;
    let stat = fstat(&fd).map_err(|error| io_error(path, error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(PortableImportError::SourceNotDirectory(path.to_path_buf()));
    }
    Ok(fd)
}

fn open_zip(path: &Path, limits: &ImportLimits) -> Result<(File, Stat), PortableImportError> {
    let fd = openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
    )
    .map_err(|error| io_error(path, error.into()))?;
    let stat = fstat(&fd).map_err(|error| io_error(path, error.into()))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(PortableImportError::SourceNotRegularFile(
            path.to_path_buf(),
        ));
    }
    if stat.st_nlink > 1 {
        return Err(PortableImportError::HardLink(path.to_path_buf()));
    }
    if stat.st_size < 0 || stat.st_size as u64 > limits.max_zip_bytes {
        return Err(PortableImportError::ZipTooLarge {
            bytes: stat.st_size.max(0) as u64,
            limit: limits.max_zip_bytes,
        });
    }
    Ok((File::from(fd), stat))
}

fn reject_source_output_overlap(
    source: &PortableSource,
    image_parent: &Path,
    runtime_root: &Path,
) -> Result<(), PortableImportError> {
    let source_path = fs::canonicalize(source.path())
        .map_err(|source_error| io_error(source.path(), source_error))?;
    let image_parent = canonicalize_target(image_parent)?;
    let runtime_root = canonicalize_target(runtime_root)?;
    let overlaps = match source {
        PortableSource::Directory(_) => {
            paths_overlap(&source_path, &image_parent) || paths_overlap(&source_path, &runtime_root)
        }
        PortableSource::Zip(_) | PortableSource::Archive(_) => {
            source_path.starts_with(&image_parent) || source_path.starts_with(&runtime_root)
        }
    };
    if overlaps {
        return Err(PortableImportError::SourceOverlapsOutput(source_path));
    }
    Ok(())
}

fn canonicalize_target(path: &Path) -> Result<PathBuf, PortableImportError> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|source| io_error(path, source));
    }
    let parent = path
        .parent()
        .ok_or_else(|| PortableImportError::MissingImageParent(path.to_path_buf()))?;
    let parent = canonicalize_target(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| PortableImportError::InvalidImageName(path.to_path_buf()))?;
    Ok(parent.join(name))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_windows_relative(
    path: &Path,
    limits: &ImportLimits,
) -> Result<String, PortableImportError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(PortableImportError::UnsafeWindowsPath(
            path.to_string_lossy().into_owned(),
        ));
    }
    let mut components = Vec::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(PortableImportError::UnsafeWindowsPath(
                path.to_string_lossy().into_owned(),
            ));
        };
        let text = component
            .to_str()
            .ok_or_else(|| PortableImportError::NonUtf8Path(path.to_path_buf()))?;
        if text.is_empty()
            || text.len() > limits.max_component_bytes
            || text.ends_with(['.', ' '])
            || text.chars().any(|character| {
                character < '\u{20}'
                    || matches!(
                        character,
                        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                    )
            })
            || is_reserved_windows_name(text)
        {
            return Err(PortableImportError::UnsafeWindowsPath(
                path.to_string_lossy().into_owned(),
            ));
        }
        components.push(text.to_owned());
    }
    if components.is_empty() || components.len() > limits.max_depth {
        return Err(PortableImportError::PathTooDeep {
            path: path.to_path_buf(),
            limit: limits.max_depth,
        });
    }
    let rendered = components.join("/");
    if rendered.len() > limits.max_path_bytes {
        return Err(PortableImportError::PathTooLong {
            path: path.to_path_buf(),
            limit: limits.max_path_bytes,
        });
    }
    Ok(components
        .into_iter()
        .map(|component| component.to_lowercase())
        .collect::<Vec<_>>()
        .join("/"))
}

fn is_reserved_windows_name(component: &str) -> bool {
    let base = component
        .split_once('.')
        .map_or(component, |(base, _)| base)
        .to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

fn register_windows_path(
    normalized: &str,
    is_directory: bool,
    explicit: bool,
    paths: &mut BTreeMap<String, bool>,
    explicit_paths: &mut BTreeSet<String>,
) -> Result<(), PortableImportError> {
    if explicit && !explicit_paths.insert(normalized.to_owned()) {
        return Err(PortableImportError::WindowsPathCollision(normalized.into()));
    }
    let components: Vec<_> = normalized.split('/').collect();
    for end in 1..components.len() {
        let parent = components[..end].join("/");
        if paths.get(&parent).is_some_and(|directory| !directory) {
            return Err(PortableImportError::WindowsPathCollision(normalized.into()));
        }
        paths.entry(parent).or_insert(true);
    }
    if let Some(existing_directory) = paths.get(normalized) {
        if *existing_directory != is_directory || !is_directory {
            return Err(PortableImportError::WindowsPathCollision(normalized.into()));
        }
    } else {
        paths.insert(normalized.to_owned(), is_directory);
    }
    Ok(())
}

fn validate_manifest_prefix_conflicts(
    paths: &BTreeMap<String, bool>,
) -> Result<(), PortableImportError> {
    for (path, is_directory) in paths {
        if !is_directory {
            let prefix = format!("{path}/");
            if paths
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(candidate, _)| candidate.starts_with(&prefix))
            {
                return Err(PortableImportError::WindowsPathCollision(path.clone()));
            }
        }
    }
    Ok(())
}

fn copy_bounded<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    expected: u64,
    limit: u64,
    path: &Path,
    mut first_bytes: Option<&mut [u8]>,
) -> Result<u64, PortableImportError> {
    let maximum = expected.min(limit);
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let remaining = maximum.saturating_sub(copied);
        let allowance = remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let read = reader
            .read(&mut buffer[..allowance])
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        if copied + read as u64 > limit || copied + read as u64 > expected {
            return Err(PortableImportError::StreamLimitExceeded(path.to_path_buf()));
        }
        if let Some(bytes) = first_bytes.as_deref_mut() {
            for (offset, byte) in buffer[..read].iter().copied().enumerate() {
                let position = copied as usize + offset;
                if position < bytes.len() {
                    bytes[position] = byte;
                }
            }
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|source| io_error(path, source))?;
        copied += read as u64;
    }
    Ok(copied)
}

fn has_launcher_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("exe")
                || extension.eq_ignore_ascii_case("sh")
                || extension.eq_ignore_ascii_case("appimage")
        })
}

fn classify_launcher(path: &Path, prefix: &[u8]) -> Option<RunnerKind> {
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or("");
    if extension.eq_ignore_ascii_case("exe") && prefix.starts_with(b"MZ") {
        Some(RunnerKind::Wine)
    } else if extension.eq_ignore_ascii_case("sh") && prefix.starts_with(b"#!")
        || extension.eq_ignore_ascii_case("appimage") && prefix.starts_with(b"\x7fELF")
    {
        Some(RunnerKind::Native)
    } else {
        None
    }
}

fn portable_file_mode(archived_mode: u32, prefix: &[u8]) -> u32 {
    let inferred_executable = prefix.starts_with(b"\x7fELF") || prefix.starts_with(b"#!");
    0o644 | (archived_mode & 0o111) | if inferred_executable { 0o111 } else { 0 }
}

fn suggested_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Imported game")
        .to_owned()
}

fn recommend_candidate(inspection: &PortableInspection) -> usize {
    let source_advertises_english = inspection
        .suggested_name
        .to_ascii_lowercase()
        .contains("en");
    inspection
        .executable_candidates
        .iter()
        .enumerate()
        .min_by_key(|(index, candidate)| {
            let depth = candidate.components().count() as i32;
            let stem = candidate
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let mut score = depth * 10;
            if inspection.candidate_runners.get(*index) == Some(&RunnerKind::Native) {
                score -= 200;
            }
            if [
                "setup",
                "install",
                "installer",
                "uninstall",
                "unins",
                "config",
            ]
            .iter()
            .any(|word| stem.contains(word))
            {
                score += 1_000;
            }
            if source_advertises_english && (stem.ends_with("_en") || stem.ends_with("-en")) {
                score -= 100;
            }
            (score, candidate.to_string_lossy().to_ascii_lowercase())
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn same_identity_and_times(before: &Stat, after: &Stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_mode == after.st_mode
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

#[derive(Debug, thiserror::Error)]
pub enum PortableImportError {
    #[error(transparent)]
    InvalidPath(#[from] crate::backend::PathValidationError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("source is not a real directory: {0:?}")]
    SourceNotDirectory(PathBuf),
    #[error("source is not a regular archive file: {0:?}")]
    SourceNotRegularFile(PathBuf),
    #[error("source contains a hard-linked file, which portable import rejects: {0:?}")]
    HardLink(PathBuf),
    #[error("source contains a symlink, device, socket or FIFO: {0:?}")]
    UnsupportedFileType(PathBuf),
    #[error("ZIP entry {0} is a symlink")]
    ZipLink(usize),
    #[error("ZIP entry {index} has unsupported Unix mode {mode:o}: {path:?}")]
    UnsupportedZipFileType {
        path: PathBuf,
        mode: u32,
        index: usize,
    },
    #[error("ZIP entry {0} is encrypted")]
    EncryptedZipEntry(usize),
    #[error("ZIP entry {index} uses unsupported compression {method}")]
    UnsupportedCompression { index: usize, method: String },
    #[error("ZIP entry {0} does not have an unambiguous UTF-8 path")]
    NonUtf8ZipPath(usize),
    #[error("source path is not valid on Windows: {0}")]
    UnsafeWindowsPath(String),
    #[error("source path is not UTF-8: {0:?}")]
    NonUtf8Path(PathBuf),
    #[error("source path is deeper than {limit} components: {path:?}")]
    PathTooDeep { path: PathBuf, limit: usize },
    #[error("source path exceeds {limit} bytes: {path:?}")]
    PathTooLong { path: PathBuf, limit: usize },
    #[error("source has two paths that collide on Windows: {0}")]
    WindowsPathCollision(String),
    #[error("source exceeds the {0}-entry limit")]
    TooManyEntries(u64),
    #[error("source has more than {0} Windows executable candidates")]
    TooManyExecutableCandidates(u64),
    #[error("file exceeds the {limit}-byte limit ({bytes} bytes): {path:?}")]
    FileTooLarge {
        path: PathBuf,
        bytes: u64,
        limit: u64,
    },
    #[error("payload exceeds the {limit}-byte limit ({bytes} bytes)")]
    PayloadTooLarge { bytes: u64, limit: u64 },
    #[error("ZIP exceeds the {limit}-byte input limit ({bytes} bytes)")]
    ZipTooLarge { bytes: u64, limit: u64 },
    #[error("archive volumes exceed the {limit}-byte input limit ({bytes} bytes)")]
    ArchiveTooLarge { bytes: u64, limit: u64 },
    #[error("ZIP central directory exceeds the {limit}-byte limit ({bytes} bytes)")]
    CentralDirectoryTooLarge { bytes: u64, limit: u64 },
    #[error("ZIP compression ratio is too high: {0:?}")]
    CompressionRatioExceeded(PathBuf),
    #[error("ZIP is malformed or unsupported: {0}")]
    MalformedZip(String),
    #[error("ZIP reader failed: {0}")]
    Zip(#[source] zip::result::ZipError),
    #[error("archive name is not usable: {0:?}")]
    UnsupportedArchiveName(PathBuf),
    #[error("archive name is not UTF-8: {0:?}")]
    NonUtf8ArchiveName(PathBuf),
    #[error("missing archive volume; keep every part in one folder: {0:?}")]
    MissingArchiveVolume(PathBuf),
    #[error("archive volume set is ambiguous: {0:?}")]
    AmbiguousArchiveVolumes(PathBuf),
    #[error("archives may contain at most {0} parts")]
    TooManyArchiveVolumes(u32),
    #[error("archive signature does not match its format: {0:?}")]
    InvalidArchiveSignature(PathBuf),
    #[error("archive volume has an invalid size ({bytes} bytes): {path:?}")]
    InvalidArchiveVolumeSize { path: PathBuf, bytes: u64 },
    #[error(
        "download is not finished yet; wait for the browser to finish and choose the archive again"
    )]
    DownloadIncomplete(PathBuf),
    #[error("the required trusted archive tool is unavailable: {0:?}")]
    ArchiveToolUnavailable(PathBuf),
    #[error("archive listing is not valid UTF-8")]
    NonUtf8ArchiveListing,
    #[error("archive listing is malformed: {0}")]
    MalformedArchiveListing(String),
    #[error("encrypted archives are not supported")]
    EncryptedArchive,
    #[error("archive entry is a link, device, socket, FIFO, or anti-item: {0:?}")]
    UnsupportedArchiveFileType(PathBuf),
    #[error("archive payload exceeds the configured compression-ratio limit")]
    ArchiveCompressionRatioExceeded,
    #[error("archive extraction did not match the validated post-extraction tree")]
    ArchiveExtractionMismatch,
    #[error("archive extraction destination is not empty: {0:?}")]
    ArchiveDestinationNotEmpty(PathBuf),
    #[error("archive listing exceeded the {0}-byte output limit")]
    ArchiveListingTooLarge(u64),
    #[error("archive diagnostics exceeded the {0}-byte output limit")]
    ArchiveDiagnosticTooLarge(u64),
    #[error("the archive output reader stopped unexpectedly")]
    ArchiveOutputWorkerFailed,
    #[error("archive inspection exceeded its overall time limit")]
    ArchiveInspectionTimedOut,
    #[error("archive tool failed with status {status:?}: {diagnostic}")]
    ArchiveToolFailed {
        status: Option<i32>,
        diagnostic: String,
    },
    #[error("archive tool could not {operation}: {source}")]
    ArchiveToolIo {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("source changed while Capsule was inspecting or copying it: {0:?}")]
    SourceChanged(PathBuf),
    #[error("source has an invalid file size: {0:?}")]
    InvalidFileSize(PathBuf),
    #[error("source data exceeded its declared or configured limit: {0:?}")]
    StreamLimitExceeded(PathBuf),
    #[error("ZIP entry size changed for {path:?}: declared {declared}, actual {actual}")]
    ZipSizeMismatch {
        path: PathBuf,
        declared: u64,
        actual: u64,
    },
    #[error("ZIP payload size changed: declared {declared}, actual {actual}")]
    ZipTotalSizeMismatch { declared: u64, actual: u64 },
    #[error("no supported Windows or Linux launchers were found")]
    NoWindowsExecutables,
    #[error("source overlaps Capsule output or runtime storage: {0:?}")]
    SourceOverlapsOutput(PathBuf),
    #[error("capsule destination already exists: {0:?}")]
    DestinationExists(PathBuf),
    #[error("capsule image has no parent directory: {0:?}")]
    MissingImageParent(PathBuf),
    #[error("capsule image name is invalid: {0:?}")]
    InvalidImageName(PathBuf),
    #[error("payload is {bytes} bytes but this image allows at most {budget} bytes")]
    PayloadDoesNotFit { bytes: u64, budget: u64 },
    #[error("not enough free space: need {required} bytes, have {available} bytes")]
    InsufficientSpace { required: u64, available: u64 },
    #[error("image size overflow")]
    ImageSizeOverflow,
    #[error("size calculation overflow")]
    SizeOverflow,
    #[error("failed to encode capsule manifest: {0}")]
    Manifest(#[source] serde_json::Error),
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> PortableImportError {
    PortableImportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, contents) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(contents).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn directory_inspection_finds_only_real_mz_executables() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("bin")).unwrap();
        fs::write(temp.path().join("bin/Game.exe"), b"MZfixture").unwrap();
        fs::write(temp.path().join("bin/pretend.exe"), b"not a PE").unwrap();
        fs::write(temp.path().join("readme.txt"), b"hello").unwrap();

        let result = inspect_portable_source(
            &PortableSource::Directory(temp.path().to_path_buf()),
            &ImportLimits::default(),
        )
        .unwrap();
        assert_eq!(
            result.executable_candidates,
            [PathBuf::from("drive_c/Game/bin/Game.exe")]
        );
        assert_eq!(result.entries, 4);
    }

    #[test]
    fn dual_platform_game_prefers_its_native_linux_launcher() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("Game.exe"), b"MZfixture").unwrap();
        fs::write(temp.path().join("Game.sh"), b"#!/bin/sh\nexit 0\n").unwrap();

        let result = inspect_portable_source(
            &PortableSource::Directory(temp.path().to_path_buf()),
            &ImportLimits::default(),
        )
        .unwrap();

        assert_eq!(
            result.candidate(result.recommended_candidate),
            Some((Path::new("drive_c/Game/Game.sh"), RunnerKind::Native))
        );
        assert!(result.candidate_runners.contains(&RunnerKind::Wine));
        assert!(result.candidate_runners.contains(&RunnerKind::Native));
    }

    #[test]
    fn copied_native_payload_restores_elf_and_shebang_execute_bits() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("lib/linux-x86_64")).unwrap();
        fs::write(
            source.path().join("Game.sh"),
            b"#!/bin/sh\nexec ./lib/linux-x86_64/Game\n",
        )
        .unwrap();
        fs::write(
            source.path().join("lib/linux-x86_64/Game"),
            b"\x7fELFfixture",
        )
        .unwrap();

        inspect_directory(
            source.path(),
            &ImportLimits::default(),
            Some(destination.path()),
        )
        .unwrap();

        for relative in ["Game.sh", "lib/linux-x86_64/Game"] {
            let mode = fs::metadata(destination.path().join(relative))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "{relative} was not executable");
        }
    }

    #[test]
    fn external_archive_rescan_repairs_extensionless_elf_mode() {
        let extracted = tempfile::tempdir().unwrap();
        let runtime = extracted.path().join("lib/linux-x86_64/Game");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(&runtime, b"\x7fELFfixture").unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o644)).unwrap();

        scan_directory(extracted.path(), &ImportLimits::default(), None, true).unwrap();

        assert_eq!(
            fs::metadata(runtime).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }

    #[test]
    fn directory_inspection_rejects_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        symlink("/etc/passwd", temp.path().join("game.exe")).unwrap();
        let error = inspect_portable_source(
            &PortableSource::Directory(temp.path().to_path_buf()),
            &ImportLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, PortableImportError::Io { .. }));
    }

    #[test]
    fn zip_inspection_finds_executables_and_prefers_english_variant() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("Game_EN.zip");
        write_zip(
            &archive,
            &[
                ("Game/Game.exe", b"MZdefault"),
                ("Game/Game_EN.EXE", b"MZenglish"),
                ("Game/data.bin", b"data"),
            ],
        );

        let result =
            inspect_portable_source(&PortableSource::Zip(archive), &ImportLimits::default())
                .unwrap();
        assert_eq!(result.executable_candidates.len(), 2);
        assert_eq!(
            result.executable_candidates[result.recommended_candidate],
            PathBuf::from("drive_c/Game/Game/Game_EN.EXE")
        );
    }

    #[test]
    fn zip_copy_restores_native_runtime_execute_bits_without_unix_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let archive = temp.path().join("Native.zip");
        write_zip(
            &archive,
            &[
                ("Game.sh", b"#!/bin/sh\nexec ./lib/linux-x86_64/Game\n"),
                ("lib/linux-x86_64/Game", b"\x7fELFfixture"),
            ],
        );

        inspect_zip(&archive, &ImportLimits::default(), Some(destination.path())).unwrap();

        for relative in ["Game.sh", "lib/linux-x86_64/Game"] {
            let mode = fs::metadata(destination.path().join(relative))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "{relative} was not executable");
        }
    }

    #[test]
    fn zip_inspection_rejects_windows_case_collisions() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("collision.zip");
        write_zip(&archive, &[("Game.exe", b"MZone"), ("game.EXE", b"MZtwo")]);
        let error =
            inspect_portable_source(&PortableSource::Zip(archive), &ImportLimits::default())
                .unwrap_err();
        assert!(matches!(
            error,
            PortableImportError::WindowsPathCollision(_)
        ));
    }

    #[test]
    fn windows_path_validation_rejects_traversal_and_device_names() {
        let limits = ImportLimits::default();
        assert!(validate_windows_relative(Path::new("../game.exe"), &limits).is_err());
        assert!(validate_windows_relative(Path::new("CON.txt"), &limits).is_err());
        assert!(validate_windows_relative(Path::new("folder/name. "), &limits).is_err());
        assert!(validate_windows_relative(Path::new("folder/game.exe"), &limits).is_ok());
    }

    #[test]
    fn portable_image_capacity_tracks_payload_instead_of_using_a_fixed_32_gib() {
        assert_eq!(recommended_image_size_mib(0), 1_024);
        assert_eq!(recommended_image_size_mib(618_948_090), 1_792);
        assert_eq!(recommended_image_size_mib(4 * GIB), 5_632);
        assert_eq!(recommended_image_size_mib(24 * GIB), 32_768);

        for payload in [1, 618_948_090, 4 * GIB, 24 * GIB] {
            let capacity = recommended_image_size_mib(payload) * MIB;
            assert!(payload <= capacity.saturating_mul(3) / 4);
        }
    }

    fn write_fake_7z_part(path: &Path, tail: &[u8]) {
        let mut bytes = SEVEN_ZIP_SIGNATURE.to_vec();
        bytes.extend_from_slice(tail);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn split_7z_selection_resolves_any_part_to_a_contiguous_set() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("Game.7z.001");
        let second = temp.path().join("Game.7z.002");
        write_fake_7z_part(&first, b"one");
        fs::write(&second, b"two").unwrap();

        let resolved = resolve_archive_parts(&second).unwrap();
        assert_eq!(resolved.primary, first);
        assert_eq!(resolved.parts, [first, second]);
        assert_eq!(resolved.suggested_name, "Game");
    }

    #[test]
    fn split_7z_selection_accepts_uppercase_extension() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("Game.7Z.001");
        let second = temp.path().join("Game.7Z.002");
        write_fake_7z_part(&first, b"one");
        fs::write(&second, b"two").unwrap();

        let resolved = resolve_archive_parts(&second).unwrap();
        assert_eq!(resolved.primary, first);
        assert_eq!(resolved.parts, [first, second]);
        assert_eq!(resolved.suggested_name, "Game");
    }

    #[test]
    fn split_7z_selection_reports_a_missing_middle_part() {
        let temp = tempfile::tempdir().unwrap();
        write_fake_7z_part(&temp.path().join("Game.7z.001"), b"one");
        let third = temp.path().join("Game.7z.003");
        fs::write(&third, b"three").unwrap();

        let error = resolve_archive_parts(&third).unwrap_err();
        assert!(
            matches!(error, PortableImportError::MissingArchiveVolume(path)
            if path.ends_with("Game.7z.002"))
        );
    }

    #[test]
    fn split_7z_selection_rejects_a_part_beyond_the_bounded_set() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("Game.7z.001");
        write_fake_7z_part(&first, b"one");
        fs::write(temp.path().join("Game.7z.129"), b"overflow").unwrap();

        assert!(matches!(
            resolve_archive_parts(&first),
            Err(PortableImportError::TooManyArchiveVolumes(128))
        ));
    }

    #[test]
    fn archive_source_accepts_full_engine_formats_and_numbered_sets() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "Game.rar",
            "Game.cab",
            "Game.tar.gz",
            "Game.zipx",
            "Game.iso",
        ] {
            let path = temp.path().join(name);
            fs::write(&path, b"not inspected by name resolution").unwrap();
            let resolved = resolve_archive_parts(&path).unwrap();
            assert_eq!(resolved.primary, path);
            assert_eq!(resolved.parts, [path]);
            assert_eq!(resolved.suggested_name, "Game");
        }

        let first = temp.path().join("Bundle.zip.001");
        let second = temp.path().join("Bundle.zip.002");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        let resolved = resolve_archive_parts(&second).unwrap();
        assert_eq!(resolved.primary, first);
        assert_eq!(resolved.parts, [first, second]);
        assert_eq!(resolved.suggested_name, "Bundle");
    }

    #[test]
    fn zero_byte_browser_target_reports_an_incomplete_download() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("Goodbye_Checklist.zip");
        fs::write(&target, b"").unwrap();
        fs::write(
            temp.path().join("Goodbye_Checklist.fFBambxr.zip.part"),
            b"partial bytes",
        )
        .unwrap();
        let resolved = resolve_archive_parts(&target).unwrap();
        assert!(matches!(
            open_archive_parts(&resolved, &ImportLimits::default()),
            Err(PortableImportError::DownloadIncomplete(path)) if path == target
        ));
    }

    #[test]
    fn tor_partial_token_can_precede_an_earlier_filename_suffix() {
        let target = OsStr::new("May's Summer Vacation v0.06.0b public uncensored.rar");
        let partial =
            OsStr::new("May's Summer Vacation v0.ZVy0I8Hp.06.0b public uncensored.rar.part");
        assert!(partial_download_name_matches(target, partial));
        assert!(!partial_download_name_matches(
            target,
            OsStr::new("Another.ZVy0I8Hp.rar.part")
        ));
    }

    #[test]
    fn archive_passwords_use_stdin_and_are_redacted() {
        let password = ArchivePassword::new("correct horse battery staple".into());
        assert_eq!(format!("{password:?}"), "ArchivePassword([REDACTED])");

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("IFS= read -r supplied; [ \"$supplied\" = \"correct horse battery staple\" ] && printf accepted");
        let output = run_archive_command(command, 64, Some(&password)).unwrap();
        assert_eq!(output, b"accepted");
    }

    #[test]
    fn password_prompt_on_stdout_is_not_reported_as_break_signaled() {
        assert!(archive_output_mentions_password(
            b"\nEnter password:\n",
            b"Break signaled\n"
        ));
    }

    #[test]
    fn supplied_password_prompt_is_removed_from_archive_listing() {
        let listing = "\r\nEnter password:\r\nPath = Game/game.exe\nSize = 4\n";
        assert_eq!(
            strip_archive_password_prompt(listing, true),
            "Path = Game/game.exe\nSize = 4\n"
        );
        assert_eq!(strip_archive_password_prompt(listing, false), listing);
    }

    #[test]
    fn modern_and_legacy_multipart_rar_sets_resolve_from_any_part() {
        let modern = tempfile::tempdir().unwrap();
        let first = modern.path().join("Game.part01.rar");
        let second = modern.path().join("Game.part02.rar");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        let resolved = resolve_archive_parts(&second).unwrap();
        assert_eq!(resolved.primary, first);
        assert_eq!(resolved.parts, [first, second]);
        assert_eq!(resolved.suggested_name, "Game");
        assert_eq!(resolved.signature, Some(ArchiveSignature::Rar));

        let legacy = tempfile::tempdir().unwrap();
        let primary = legacy.path().join("OldGame.rar");
        let second = legacy.path().join("OldGame.r00");
        let third = legacy.path().join("OldGame.r01");
        fs::write(&primary, b"primary").unwrap();
        fs::write(&second, b"second").unwrap();
        fs::write(&third, b"third").unwrap();
        let resolved = resolve_archive_parts(&third).unwrap();
        assert_eq!(resolved.primary, primary);
        assert_eq!(resolved.parts, [primary, second, third]);
        assert_eq!(resolved.suggested_name, "OldGame");
    }

    #[test]
    fn classic_split_zip_resolves_the_central_directory_volume() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("Game.zip");
        let first = temp.path().join("Game.z01");
        let second = temp.path().join("Game.z02");
        fs::write(&primary, b"central directory").unwrap();
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        let resolved = resolve_archive_parts(&second).unwrap();
        assert_eq!(resolved.primary, primary);
        assert_eq!(resolved.parts, [primary, first, second]);
        assert_eq!(resolved.suggested_name, "Game");
        assert!(!resolved.equal_sized_volumes);
    }

    #[test]
    fn archive_listing_reuses_windows_path_and_link_protections() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("Game.7z");
        write_fake_7z_part(&archive, b"fixture");
        let resolved = resolve_archive_parts(&archive).unwrap();
        let opened = open_archive_parts(&resolved, &ImportLimits::default()).unwrap();
        let compressed_bytes = archive_identity_total_size(&opened.identities).unwrap();
        let limits = ImportLimits::default();
        let traversal = b"Path = ../game.exe\nSize = 2\nAttributes = A -rw-r--r--\nEncrypted = -\n";
        assert!(matches!(
            parse_external_listing(traversal, &resolved, compressed_bytes, &limits, false),
            Err(PortableImportError::UnsafeWindowsPath(_))
        ));

        let link = b"Path = game.exe\nSize = 2\nAttributes = A lrwxrwxrwx\nEncrypted = -\n";
        assert!(matches!(
            parse_external_listing(link, &resolved, compressed_bytes, &limits, false),
            Err(PortableImportError::UnsupportedArchiveFileType(_))
        ));

        let collision = b"Path = Game.exe\nSize = 2\nAttributes = A -rw-r--r--\nEncrypted = -\n\nPath = game.EXE\nSize = 2\nAttributes = A -rw-r--r--\nEncrypted = -\n";
        assert!(matches!(
            parse_external_listing(collision, &resolved, compressed_bytes, &limits, false),
            Err(PortableImportError::WindowsPathCollision(_))
        ));

        let encrypted = b"Path = game.exe\nSize = 2\nAttributes = A -rw-r--r--\nEncrypted = +\n";
        assert!(matches!(
            parse_external_listing(encrypted, &resolved, compressed_bytes, &limits, false),
            Err(PortableImportError::EncryptedArchive)
        ));
        assert!(
            parse_external_listing(encrypted, &resolved, compressed_bytes, &limits, true).is_ok()
        );
    }

    #[test]
    fn archive_command_passes_only_open_part_fds_into_the_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("Game.7z.001");
        let second = temp.path().join("Game.7z.002");
        write_fake_7z_part(&first, b"one");
        fs::write(&second, b"two").unwrap();
        let resolved = resolve_archive_parts(&second).unwrap();
        let opened = open_archive_parts(&resolved, &ImportLimits::default()).unwrap();
        let command = archive_command(
            &resolved,
            &opened,
            Duration::from_secs(1),
            MIB,
            None,
            [OsString::from("i")],
        )
        .unwrap();
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| *argument == "--ro-bind-fd")
                .count(),
            2
        );
        assert!(arguments.iter().any(|argument| argument == SEVEN_ZIP_PATH));
        let bwrap_index = arguments
            .iter()
            .position(|argument| argument == BWRAP_PATH)
            .expect("timeout starts Bubblewrap directly");
        let prlimit_index = arguments
            .iter()
            .rposition(|argument| argument == PRLIMIT_PATH)
            .expect("the private archive namespace contains prlimit");
        let seven_zip_index = arguments
            .iter()
            .rposition(|argument| argument == SEVEN_ZIP_PATH)
            .expect("prlimit starts the fixed full 7-Zip executable");
        assert!(bwrap_index < prlimit_index);
        assert!(prlimit_index < seven_zip_index);
        assert!(arguments[prlimit_index + 1].starts_with("--as="));
        assert!(arguments[prlimit_index + 2].starts_with("--nproc=16"));
        assert!(arguments.windows(3).any(|bind| {
            bind[0] == "--ro-bind"
                && bind[1] == "/usr/lib/libsmartcols.so.1"
                && bind[2] == "/usr/lib/libsmartcols.so.1"
        }));
        assert!(arguments.windows(3).any(|bind| {
            bind[0] == "--ro-bind"
                && bind[1] == SEVEN_ZIP_PLUGIN_PATH
                && bind[2] == SEVEN_ZIP_PLUGIN_PATH
        }));
        let size_index = arguments
            .iter()
            .position(|argument| argument == "--size")
            .expect("the archive sandbox bounds its tmpfs");
        assert_eq!(
            arguments.get(size_index + 1).map(String::as_str),
            Some("268435456")
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == &first.to_string_lossy())
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == &second.to_string_lossy())
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == &temp.path().to_string_lossy())
        );
    }

    #[test]
    fn external_extraction_uses_relative_path_mode_supported_by_current_7zip() {
        let arguments = external_extraction_arguments(PathBuf::from("/input/Game.7z"));
        assert!(!arguments.iter().any(|argument| {
            argument
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("-spf")
        }));
        assert!(arguments.windows(3).any(|window| {
            window[0] == "-o/output" && window[1] == "--" && window[2] == "/input/Game.7z"
        }));
    }

    #[test]
    fn held_archive_snapshot_detects_in_place_changes() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("Game.7z");
        write_fake_7z_part(&archive, b"before");
        let resolved = resolve_archive_parts(&archive).unwrap();
        let opened = open_archive_parts(&resolved, &ImportLimits::default()).unwrap();
        let before = opened.identities.clone();

        write_fake_7z_part(&archive, b"changed payload");
        let after = snapshot_opened_archive(&opened).unwrap();

        assert_ne!(before, after);
    }

    #[test]
    fn expired_archive_inspection_deadline_fails_closed() {
        let deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            remaining_inspection_time(deadline),
            Err(PortableImportError::ArchiveInspectionTimedOut)
        ));
    }

    #[test]
    #[ignore = "set CAPSULE_TEST_ZIP to inspect a real external archive"]
    fn inspects_an_external_portable_zip() {
        let archive = std::env::var_os("CAPSULE_TEST_ZIP").expect("CAPSULE_TEST_ZIP is required");
        let result = inspect_portable_source(
            &PortableSource::Zip(PathBuf::from(archive)),
            &ImportLimits::default(),
        )
        .unwrap();
        assert!(result.entries > 0);
        let recommended = &result.executable_candidates[result.recommended_candidate];
        assert_eq!(
            recommended
                .extension()
                .and_then(|extension| extension.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("exe")
        );
    }

    #[test]
    #[ignore = "set CAPSULE_TEST_ARCHIVE and optionally CAPSULE_TEST_ARCHIVE_PASSWORD"]
    fn inspects_external_archive_with_full_sandboxed_engine() {
        let archive =
            std::env::var_os("CAPSULE_TEST_ARCHIVE").expect("CAPSULE_TEST_ARCHIVE is required");
        let path = PathBuf::from(archive);
        let password = std::env::var("CAPSULE_TEST_ARCHIVE_PASSWORD")
            .ok()
            .map(ArchivePassword::new);
        let destination = tempfile::tempdir().unwrap();
        let result = inspect_external_archive(
            &path,
            &ImportLimits::default(),
            Some(destination.path()),
            password.as_ref(),
        );
        if password.is_none() && matches!(result, Err(PortableImportError::EncryptedArchive)) {
            eprintln!("external full-engine inspection correctly requested an archive password");
            return;
        }
        let inspection =
            result.expect("the sandboxed full archive engine should inspect the selected archive");
        assert!(inspection.entries > 0);
        eprintln!("external full-engine inspection: {inspection:#?}");
    }
}
