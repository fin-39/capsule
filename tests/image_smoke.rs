#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::os::unix::fs::symlink;

use capsule::backend::capabilities::{Capability, CapabilityReport};
use capsule::backend::importer::{ImportRequest, import_prepared_prefix};
use capsule::backend::portable::{
    ImportLimits, PortableImportRequest, PortableSource, import_portable_game,
};
use capsule::backend::storage::ImageMountPlan;
use uuid::Uuid;

fn fuse_is_usable(capabilities: &CapabilityReport) -> bool {
    [
        Capability::Fuse2fs,
        Capability::Fusermount,
        Capability::MkfsExt4,
    ]
    .into_iter()
    .all(|capability| capabilities.has(capability))
        && OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fuse")
            .is_ok()
}

#[test]
fn creates_mounts_and_persists_a_single_file_capsule_when_fuse_is_available() {
    let capabilities = CapabilityReport::detect();
    if !fuse_is_usable(&capabilities) {
        eprintln!("skipping image smoke test: rootless FUSE is unavailable");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source-prefix");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("drive_c")).unwrap();
    fs::write(source.join("drive_c/game.exe"), b"MZ test fixture").unwrap();
    fs::create_dir(source.join("dosdevices")).unwrap();
    symlink("../drive_c", source.join("dosdevices/c:")).unwrap();
    symlink("/", source.join("dosdevices/z:")).unwrap();
    fs::write(source.join("system.reg"), b"WINE REGISTRY Version 2\n").unwrap();

    let image = temp.path().join("Smoke.capsule");
    let runtime = temp.path().join("runtime");
    let request = ImportRequest {
        id: Uuid::new_v4(),
        name: "Smoke".into(),
        source_prefix: source,
        image_path: image.clone(),
        image_size_mib: 64,
        runtime_root: runtime.clone(),
    };
    import_prepared_prefix(&request, &capabilities).unwrap();
    assert!(image.is_file());

    let mount_point = runtime.join("verify-root");
    let mount = ImageMountPlan::new(&image, &mount_point, &capabilities).unwrap();
    mount.execute_mount().unwrap();
    assert_eq!(
        fs::read(mount_point.join("prefix/drive_c/game.exe")).unwrap(),
        b"MZ test fixture"
    );
    assert!(!mount_point.join("prefix/dosdevices/z:").exists());
    mount.execute_unmount().unwrap();
}

#[test]
fn packages_a_portable_game_without_running_it_when_fuse_is_available() {
    let capabilities = CapabilityReport::detect();
    if !fuse_is_usable(&capabilities) {
        eprintln!("skipping portable image smoke test: rootless FUSE is unavailable");
        return;
    }

    let source_temp = tempfile::tempdir().unwrap();
    let output_temp = tempfile::tempdir().unwrap();
    let source = source_temp.path().join("Portable Game");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("bin")).unwrap();
    fs::write(source.join("bin/Game.exe"), b"MZ portable fixture").unwrap();
    fs::write(source.join("data.bin"), b"contained data").unwrap();

    let image = output_temp.path().join("Portable.capsule");
    let runtime = output_temp.path().join("runtime");
    let request = PortableImportRequest {
        id: Uuid::new_v4(),
        name: "Portable".into(),
        source: PortableSource::Directory(source),
        archive_password: None,
        image_path: image.clone(),
        image_size_mib: 64,
        runtime_root: runtime.clone(),
        limits: ImportLimits::default(),
    };
    let result = import_portable_game(&request, &capabilities).unwrap();
    assert_eq!(
        result.inspection.executable_candidates,
        [std::path::PathBuf::from("drive_c/Game/bin/Game.exe")]
    );

    let mount_point = runtime.join("verify-portable-root");
    let mount = ImageMountPlan::new(&image, &mount_point, &capabilities).unwrap();
    mount.execute_mount().unwrap();
    assert_eq!(
        fs::read(mount_point.join("prefix/drive_c/Game/bin/Game.exe")).unwrap(),
        b"MZ portable fixture"
    );
    assert!(!mount_point.join("prefix/system.reg").exists());
    mount.execute_unmount().unwrap();
}

#[test]
#[ignore = "set CAPSULE_TEST_ZIP to package a real external archive"]
fn packages_an_external_portable_zip_without_running_it() {
    let archive = std::env::var_os("CAPSULE_TEST_ZIP").expect("CAPSULE_TEST_ZIP is required");
    let capabilities = CapabilityReport::detect();
    assert!(
        [
            Capability::Fuse2fs,
            Capability::Fusermount,
            Capability::MkfsExt4,
        ]
        .into_iter()
        .all(|capability| capabilities.has(capability))
    );

    let output = tempfile::tempdir().unwrap();
    let image = output.path().join("Portable-Game.capsule");
    let runtime = output.path().join("runtime");
    let request = PortableImportRequest {
        id: Uuid::new_v4(),
        name: "Portable Game".into(),
        source: PortableSource::Zip(archive.into()),
        archive_password: None,
        image_path: image.clone(),
        image_size_mib: 1_024,
        runtime_root: runtime.clone(),
        limits: ImportLimits::default(),
    };
    let result = import_portable_game(&request, &capabilities).unwrap();
    let executable_path =
        &result.inspection.executable_candidates[result.inspection.recommended_candidate];

    let mount_point = runtime.join("verify-portable-game-root");
    let mount = ImageMountPlan::new(&image, &mount_point, &capabilities).unwrap();
    mount.execute_mount().unwrap();
    let executable = fs::read(mount_point.join("prefix").join(executable_path)).unwrap();
    assert_eq!(&executable[..2], b"MZ");
    mount.execute_unmount().unwrap();
}
