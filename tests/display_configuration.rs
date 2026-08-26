use capsule::model::{
    CapsuleRecord, DEFAULT_WINE_VIRTUAL_DESKTOP, RunnerKind, StorageKind, WineVirtualDesktop,
};
use capsule::store::LibraryStore;

const LEGACY_MODE_DIALOG_MIN_WIDTH: u32 = 640;
const LEGACY_MODE_DIALOG_MIN_HEIGHT: u32 = 480;

fn legacy_game_can_show_mode_dialog(desktop: WineVirtualDesktop) -> bool {
    desktop.width > LEGACY_MODE_DIALOG_MIN_WIDTH && desktop.height > LEGACY_MODE_DIALOG_MIN_HEIGHT
}

#[test]
fn legacy_mode_dialog_requires_a_desktop_larger_than_640x480() {
    // Some fixed-size games check both dimensions using strict greater-than
    // comparisons. The old 640x480 setting is valid Wine configuration, but
    // can suppress their own window-mode question.
    assert!(!legacy_game_can_show_mode_dialog(WineVirtualDesktop {
        width: 640,
        height: 480,
    }));
    assert!(legacy_game_can_show_mode_dialog(
        DEFAULT_WINE_VIRTUAL_DESKTOP
    ));
}

#[test]
fn legacy_compatibility_desktop_survives_library_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let store = LibraryStore::new(directory.path().join("library.json"));
    let desktop = DEFAULT_WINE_VIRTUAL_DESKTOP;

    let mut record = CapsuleRecord::new(
        "Legacy Game",
        StorageKind::Image {
            path: directory.path().join("Legacy-Game.capsule"),
        },
        "drive_c/Game/bin/GAME.EXE",
        RunnerKind::Wine,
    );
    record.wine_virtual_desktop = Some(desktop);

    let created = store.create(record).unwrap();
    let reloaded = store.get(created.id).unwrap();

    assert_eq!(reloaded.wine_virtual_desktop, Some(desktop));
    assert!(legacy_game_can_show_mode_dialog(
        reloaded.wine_virtual_desktop.unwrap()
    ));
}
