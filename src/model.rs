use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

/// The on-disk JSON schema understood by this version of Capsule.
pub const LIBRARY_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    #[default]
    Wine,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StorageKind {
    /// A single capsule filesystem image created and owned by Capsule.
    Image { path: PathBuf },
    /// A capsule image registered in place. Removing its library entry must
    /// never delete or move the underlying user-owned file.
    ExternalImage { path: PathBuf },
    /// An unpacked directory, intended for development and debugging only.
    DirectoryDev { path: PathBuf },
}

impl StorageKind {
    pub fn path(&self) -> &Path {
        match self {
            Self::Image { path } | Self::ExternalImage { path } | Self::DirectoryDev { path } => {
                path
            }
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. } | Self::ExternalImage { .. })
    }

    pub fn is_managed_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    pub fn image_path(&self) -> Option<&Path> {
        match self {
            Self::Image { path } | Self::ExternalImage { path } => Some(path),
            Self::DirectoryDev { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationProfile {
    /// No optional host integration. This is the default for unknown software.
    #[default]
    Locked,
    /// A practical game preset with no network access.
    OfflineGame,
    /// A practical game preset with internet access.
    OnlineGame,
    /// Individually configured permissions.
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Off,
    /// Outbound internet access with host loopback and private networks blocked.
    InternetOnly,
    /// Local-network access without outbound internet access.
    LanOnly,
    /// Outbound internet and local-network access.
    Lan,
    /// Outbound access restricted to the supplied host or host:port entries.
    Custom { allowed_endpoints: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AudioPolicy {
    #[default]
    Off,
    PlaybackOnly,
    PlaybackAndMicrophone,
}

/// Direct3D implementation used by contained Wine applications.
///
/// DXVK is the performance-oriented default. WineD3D remains available as a
/// per-capsule compatibility fallback for software that does not work through
/// Vulkan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WineGraphicsBackend {
    #[default]
    Dxvk,
    WineD3d,
}

impl WineGraphicsBackend {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Locale exposed to Wine for legacy non-Unicode applications.
///
/// Modern applications normally ignore this, but older localized games use
/// the process ANSI code page for menus, file names and script text. Keeping
/// it per capsule avoids changing unrelated games.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WineLocale(String);

impl WineLocale {
    pub const DEFAULT_ID: &'static str = "en_US";

    pub fn new(id: impl Into<String>) -> Result<Self, ModelError> {
        let id = id.into();
        if !is_valid_wine_locale_id(&id) {
            return Err(ModelError::InvalidWineLocale(id));
        }
        Ok(Self(id))
    }

    pub fn id(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT_ID
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if is_valid_wine_locale_id(&self.0) {
            Ok(())
        } else {
            Err(ModelError::InvalidWineLocale(self.0.clone()))
        }
    }

    pub fn japanese() -> Self {
        Self("ja_JP".into())
    }

    pub fn russian() -> Self {
        Self("ru_RU".into())
    }
}

impl Default for WineLocale {
    fn default() -> Self {
        Self(Self::DEFAULT_ID.into())
    }
}

impl Serialize for WineLocale {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WineLocale {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let stored = String::deserialize(deserializer)?;
        let id = match stored.as_str() {
            // Compatibility with the original three-option setting.
            "western" | "english" => Self::DEFAULT_ID,
            "japanese" => "ja_JP",
            "russian" => "ru_RU",
            _ => &stored,
        };
        Self::new(id).map_err(de::Error::custom)
    }
}

fn is_valid_wine_locale_id(id: &str) -> bool {
    let Some((base, modifier)) = id
        .split_once('@')
        .map_or(Some((id, None)), |(base, modifier)| {
            (!modifier.contains('@')).then_some((base, Some(modifier)))
        })
    else {
        return false;
    };

    let (language, territory) = match base.split_once('_') {
        Some((language, territory)) if !territory.contains('_') => (language, Some(territory)),
        Some(_) => return false,
        None => (base, None),
    };
    let valid_language =
        (2..=3).contains(&language.len()) && language.bytes().all(|byte| byte.is_ascii_lowercase());
    let valid_territory = territory.is_none_or(|territory| {
        territory.len() == 2 && territory.bytes().all(|byte| byte.is_ascii_uppercase())
    });
    let valid_modifier = modifier.is_none_or(|modifier| {
        !modifier.is_empty()
            && modifier.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
    });
    valid_language && valid_territory && valid_modifier
}

pub const MIN_WINE_DESKTOP_WIDTH: u32 = 640;
pub const MIN_WINE_DESKTOP_HEIGHT: u32 = 480;
pub const MAX_WINE_DESKTOP_DIMENSION: u32 = 16_384;

/// Fixed size for Wine's optional virtual desktop compatibility mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WineVirtualDesktop {
    pub width: u32,
    pub height: u32,
}

/// Compatibility size offered when virtual-desktop mode is enabled for the
/// first time. It is deliberately larger than the valid 640x480 minimum:
/// some older games only offer their own display-mode dialog when both screen
/// dimensions are strictly greater than 640x480.
pub const DEFAULT_WINE_VIRTUAL_DESKTOP: WineVirtualDesktop = WineVirtualDesktop {
    width: 800,
    height: 600,
};

impl Default for WineVirtualDesktop {
    fn default() -> Self {
        DEFAULT_WINE_VIRTUAL_DESKTOP
    }
}

impl WineVirtualDesktop {
    pub fn validate(self) -> Result<(), ModelError> {
        if self.width < MIN_WINE_DESKTOP_WIDTH
            || self.height < MIN_WINE_DESKTOP_HEIGHT
            || self.width > MAX_WINE_DESKTOP_DIMENSION
            || self.height > MAX_WINE_DESKTOP_DIMENSION
        {
            return Err(ModelError::InvalidWineDesktopSize {
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub isolation_profile: IsolationProfile,
    pub network: NetworkPolicy,
    pub audio: AudioPolicy,
    pub gpu: bool,
    pub controllers: bool,
    pub clipboard: bool,
    pub memory_limit_mib: Option<u64>,
    pub process_limit: Option<u32>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self::locked()
    }
}

impl Permissions {
    pub fn locked() -> Self {
        Self {
            isolation_profile: IsolationProfile::Locked,
            network: NetworkPolicy::Off,
            audio: AudioPolicy::Off,
            gpu: false,
            controllers: false,
            clipboard: false,
            memory_limit_mib: Some(4_096),
            process_limit: Some(512),
        }
    }

    pub fn offline_game() -> Self {
        Self {
            isolation_profile: IsolationProfile::OfflineGame,
            network: NetworkPolicy::Off,
            // The current PulseAudio bridge can also permit capture. Unknown
            // software therefore starts silent until the user accepts that
            // broader socket exposure explicitly.
            audio: AudioPolicy::Off,
            gpu: true,
            // Controller forwarding needs a selected-device broker. Exposing
            // all of /dev/input is not an acceptable preset fallback.
            controllers: false,
            clipboard: false,
            memory_limit_mib: None,
            process_limit: Some(2_048),
        }
    }

    pub fn online_game() -> Self {
        Self {
            isolation_profile: IsolationProfile::OnlineGame,
            network: NetworkPolicy::InternetOnly,
            ..Self::offline_game()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleRecord {
    pub id: Uuid,
    pub name: String,
    pub storage: StorageKind,
    /// Path to the executable inside the mounted capsule filesystem.
    pub entrypoint: PathBuf,
    /// Optional capsule-relative working directory.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub runner: RunnerKind,
    #[serde(default)]
    pub permissions: Permissions,
    /// Optional fixed-size Wine desktop for applications with window/focus
    /// compatibility problems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wine_virtual_desktop: Option<WineVirtualDesktop>,
    /// ANSI code page used by legacy Windows applications.
    #[serde(default, skip_serializing_if = "WineLocale::is_default")]
    pub wine_locale: WineLocale,
    /// Direct3D translation backend used by Wine applications.
    #[serde(default, skip_serializing_if = "WineGraphicsBackend::is_default")]
    pub wine_graphics_backend: WineGraphicsBackend,
    /// Start a Windows Steam client installed in this Wine prefix before the
    /// selected application. Steam credentials and session data consequently
    /// remain inside this capsule instead of being shared from the host.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wine_steam: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl CapsuleRecord {
    pub fn new(
        name: impl Into<String>,
        storage: StorageKind,
        entrypoint: impl Into<PathBuf>,
        runner: RunnerKind,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            storage,
            entrypoint: entrypoint.into(),
            working_dir: None,
            arguments: Vec::new(),
            runner,
            permissions: Permissions::default(),
            wine_virtual_desktop: None,
            wine_locale: WineLocale::default(),
            wine_graphics_backend: WineGraphicsBackend::default(),
            wine_steam: false,
        }
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.id.is_nil() {
            return Err(ModelError::NilId);
        }
        if self.name.trim().is_empty() {
            return Err(ModelError::EmptyName);
        }
        if self.storage.path().as_os_str().is_empty() {
            return Err(ModelError::EmptyStoragePath);
        }
        validate_capsule_relative_path("entrypoint", &self.entrypoint, false)?;
        if let Some(working_dir) = &self.working_dir {
            validate_capsule_relative_path("working directory", working_dir, true)?;
        }
        if matches!(self.permissions.memory_limit_mib, Some(0)) {
            return Err(ModelError::ZeroResourceLimit("memory"));
        }
        if matches!(self.permissions.process_limit, Some(0)) {
            return Err(ModelError::ZeroResourceLimit("process"));
        }
        if let Some(desktop) = self.wine_virtual_desktop {
            desktop.validate()?;
        }
        self.wine_locale.validate()?;
        if self.wine_steam && self.runner != RunnerKind::Wine {
            return Err(ModelError::SteamRequiresWine);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryState {
    pub version: u32,
    #[serde(default)]
    pub capsules: Vec<CapsuleRecord>,
}

impl Default for LibraryState {
    fn default() -> Self {
        Self {
            version: LIBRARY_FORMAT_VERSION,
            capsules: Vec::new(),
        }
    }
}

impl LibraryState {
    pub fn get(&self, id: Uuid) -> Option<&CapsuleRecord> {
        self.capsules.iter().find(|capsule| capsule.id == id)
    }

    pub fn get_mut(&mut self, id: Uuid) -> Option<&mut CapsuleRecord> {
        self.capsules.iter_mut().find(|capsule| capsule.id == id)
    }

    pub fn insert(&mut self, capsule: CapsuleRecord) -> Result<Uuid, ModelError> {
        capsule.validate()?;
        if self.get(capsule.id).is_some() {
            return Err(ModelError::DuplicateId(capsule.id));
        }
        let id = capsule.id;
        self.capsules.push(capsule);
        Ok(id)
    }

    pub fn replace(&mut self, capsule: CapsuleRecord) -> Result<(), ModelError> {
        capsule.validate()?;
        let id = capsule.id;
        let target = self.get_mut(id).ok_or(ModelError::NotFound(id))?;
        *target = capsule;
        Ok(())
    }

    pub fn remove(&mut self, id: Uuid) -> Result<CapsuleRecord, ModelError> {
        let index = self
            .capsules
            .iter()
            .position(|capsule| capsule.id == id)
            .ok_or(ModelError::NotFound(id))?;
        Ok(self.capsules.remove(index))
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        let mut ids = HashSet::with_capacity(self.capsules.len());
        for capsule in &self.capsules {
            capsule.validate()?;
            if !ids.insert(capsule.id) {
                return Err(ModelError::DuplicateId(capsule.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("capsule UUID cannot be nil")]
    NilId,
    #[error("capsule name cannot be empty")]
    EmptyName,
    #[error("capsule storage path cannot be empty")]
    EmptyStoragePath,
    #[error("{field} must be a capsule-relative path without '..': {path}")]
    UnsafeRelativePath { field: &'static str, path: PathBuf },
    #[error("{0} resource limit must be greater than zero")]
    ZeroResourceLimit(&'static str),
    #[error(
        "Wine virtual desktop size {width}x{height} is outside the supported range of 640x480 to 16384x16384"
    )]
    InvalidWineDesktopSize { width: u32, height: u32 },
    #[error("invalid Wine locale identifier: {0}")]
    InvalidWineLocale(String),
    #[error("the in-capsule Steam client is available only to Wine applications")]
    SteamRequiresWine,
    #[error("capsule UUID is already present in the library: {0}")]
    DuplicateId(Uuid),
    #[error("capsule was not found: {0}")]
    NotFound(Uuid),
}

fn validate_capsule_relative_path(
    field: &'static str,
    path: &Path,
    allow_empty: bool,
) -> Result<(), ModelError> {
    let unsafe_component = path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });

    if unsafe_component || (!allow_empty && path.as_os_str().is_empty()) {
        return Err(ModelError::UnsafeRelativePath {
            field,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str) -> CapsuleRecord {
        CapsuleRecord::new(
            name,
            StorageKind::Image {
                path: PathBuf::from(format!("/capsules/{name}.capsule")),
            },
            "drive_c/game.exe",
            RunnerKind::Wine,
        )
    }

    #[test]
    fn defaults_fail_closed() {
        let permissions = Permissions::default();
        assert_eq!(permissions.isolation_profile, IsolationProfile::Locked);
        assert_eq!(permissions.network, NetworkPolicy::Off);
        assert_eq!(permissions.audio, AudioPolicy::Off);
        assert!(!permissions.gpu);
        assert!(!permissions.clipboard);
    }

    #[test]
    fn older_records_default_to_safe_wine_compatibility_settings() {
        let capsule: CapsuleRecord = serde_json::from_str(
            r#"{
                "id": "98f4094d-63d9-45db-b6d3-20c7f351ea1d",
                "name": "Legacy",
                "storage": {"kind": "image", "path": "/capsules/legacy.capsule"},
                "entrypoint": "drive_c/game.exe"
            }"#,
        )
        .unwrap();

        assert_eq!(capsule.wine_virtual_desktop, None);
        assert_eq!(capsule.wine_locale, WineLocale::default());
        assert_eq!(capsule.wine_graphics_backend, WineGraphicsBackend::Dxvk);
        assert!(!capsule.wine_steam);
        capsule.validate().unwrap();
    }

    #[test]
    fn in_capsule_steam_is_explicit_and_wine_only() {
        let mut capsule = record("Steam game");
        assert!(!capsule.wine_steam);
        assert!(
            !serde_json::to_string(&capsule)
                .unwrap()
                .contains("wine_steam")
        );

        capsule.wine_steam = true;
        let json = serde_json::to_string(&capsule).unwrap();
        assert!(json.contains(r#""wine_steam":true"#));
        assert!(
            serde_json::from_str::<CapsuleRecord>(&json)
                .unwrap()
                .wine_steam
        );

        capsule.runner = RunnerKind::Native;
        assert_eq!(capsule.validate(), Err(ModelError::SteamRequiresWine));
    }

    #[test]
    fn wine_graphics_backend_defaults_to_dxvk_and_round_trips_fallback() {
        let mut capsule = record("graphics");
        assert_eq!(capsule.wine_graphics_backend, WineGraphicsBackend::Dxvk);
        assert!(
            !serde_json::to_string(&capsule)
                .unwrap()
                .contains("wine_graphics_backend")
        );

        capsule.wine_graphics_backend = WineGraphicsBackend::WineD3d;
        let json = serde_json::to_string(&capsule).unwrap();
        assert!(json.contains(r#""wine_graphics_backend":"wine_d3d""#));
        assert_eq!(
            serde_json::from_str::<CapsuleRecord>(&json)
                .unwrap()
                .wine_graphics_backend,
            WineGraphicsBackend::WineD3d
        );
    }

    #[test]
    fn non_english_wine_locales_round_trip() {
        for locale in [
            WineLocale::japanese(),
            WineLocale::russian(),
            WineLocale::new("uk_UA").unwrap(),
            WineLocale::new("sr_RS@latin").unwrap(),
        ] {
            let mut capsule = record("Legacy");
            capsule.wine_locale = locale.clone();

            let json = serde_json::to_string(&capsule).unwrap();
            let decoded: CapsuleRecord = serde_json::from_str(&json).unwrap();

            assert_eq!(decoded.wine_locale, locale);
            assert_eq!(decoded, capsule);
        }
    }

    #[test]
    fn old_locale_names_still_load() {
        for (stored, expected) in [
            (r#""western""#, WineLocale::default()),
            (r#""english""#, WineLocale::default()),
            (r#""japanese""#, WineLocale::japanese()),
            (r#""russian""#, WineLocale::russian()),
        ] {
            let locale: WineLocale = serde_json::from_str(stored).unwrap();
            assert_eq!(locale, expected);
        }
    }

    #[test]
    fn unsafe_or_malformed_wine_locale_names_are_rejected() {
        for id in [
            "",
            "C",
            "english",
            "ja-jp",
            "ja_JP.UTF-8",
            "../../tmp/x",
            "ru_RU;touch",
            "sr_RS@",
            "sr_RS@latin@extra",
        ] {
            assert!(matches!(
                WineLocale::new(id),
                Err(ModelError::InvalidWineLocale(_))
            ));
        }
    }

    #[test]
    fn default_wine_virtual_desktop_exceeds_the_legacy_minimum() {
        let desktop = WineVirtualDesktop::default();

        assert_eq!(desktop, DEFAULT_WINE_VIRTUAL_DESKTOP);
        assert_eq!(desktop.width, 800);
        assert_eq!(desktop.height, 600);
        assert!(desktop.width > MIN_WINE_DESKTOP_WIDTH);
        assert!(desktop.height > MIN_WINE_DESKTOP_HEIGHT);
        desktop.validate().unwrap();
    }

    #[test]
    fn wine_virtual_desktop_round_trips() {
        let mut capsule = record("desktop");
        capsule.wine_virtual_desktop = Some(WineVirtualDesktop {
            width: 640,
            height: 480,
        });

        let json = serde_json::to_string(&capsule).unwrap();
        let decoded: CapsuleRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, capsule);
        decoded.validate().unwrap();
    }

    #[test]
    fn wine_virtual_desktop_size_is_bounded() {
        for desktop in [
            WineVirtualDesktop {
                width: MIN_WINE_DESKTOP_WIDTH - 1,
                height: MIN_WINE_DESKTOP_HEIGHT,
            },
            WineVirtualDesktop {
                width: MIN_WINE_DESKTOP_WIDTH,
                height: MIN_WINE_DESKTOP_HEIGHT - 1,
            },
            WineVirtualDesktop {
                width: MAX_WINE_DESKTOP_DIMENSION + 1,
                height: MIN_WINE_DESKTOP_HEIGHT,
            },
            WineVirtualDesktop {
                width: MIN_WINE_DESKTOP_WIDTH,
                height: MAX_WINE_DESKTOP_DIMENSION + 1,
            },
        ] {
            let mut capsule = record("bad-size");
            capsule.wine_virtual_desktop = Some(desktop);
            assert_eq!(
                capsule.validate(),
                Err(ModelError::InvalidWineDesktopSize {
                    width: desktop.width,
                    height: desktop.height,
                })
            );
        }
    }

    #[test]
    fn capsule_paths_cannot_escape_the_capsule() {
        let mut capsule = record("unsafe");
        capsule.entrypoint = PathBuf::from("../host-command");
        assert!(matches!(
            capsule.validate(),
            Err(ModelError::UnsafeRelativePath {
                field: "entrypoint",
                ..
            })
        ));
    }

    #[test]
    fn library_crud_and_duplicate_detection() {
        let mut library = LibraryState::default();
        let mut capsule = record("Legacy Game");
        let id = capsule.id;

        assert_eq!(library.insert(capsule.clone()), Ok(id));
        assert_eq!(library.get(id).unwrap().name, "Legacy Game");
        assert_eq!(
            library.insert(capsule.clone()),
            Err(ModelError::DuplicateId(id))
        );

        capsule.name = "Legacy Game Updated".into();
        library.replace(capsule).unwrap();
        assert_eq!(library.get(id).unwrap().name, "Legacy Game Updated");
        assert_eq!(library.remove(id).unwrap().id, id);
        assert!(library.get(id).is_none());
    }

    #[test]
    fn library_json_round_trips() {
        let mut library = LibraryState::default();
        library.insert(record("Legacy Game")).unwrap();
        let json = serde_json::to_string(&library).unwrap();
        let decoded: LibraryState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, library);
    }

    #[test]
    fn external_images_are_image_backed_but_not_managed() {
        let storage = StorageKind::ExternalImage {
            path: PathBuf::from("/downloads/game.capsule"),
        };

        assert!(storage.is_image());
        assert!(!storage.is_managed_image());
        assert_eq!(
            storage.image_path(),
            Some(Path::new("/downloads/game.capsule"))
        );

        let json = serde_json::to_string(&storage).unwrap();
        let decoded: StorageKind = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, storage);
    }
}
