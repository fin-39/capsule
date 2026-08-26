use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use capsule::locale::wine_locale_choices;
use capsule::model::{
    AudioPolicy, CapsuleRecord, DEFAULT_WINE_VIRTUAL_DESKTOP, IsolationProfile,
    MAX_WINE_DESKTOP_DIMENSION, MIN_WINE_DESKTOP_HEIGHT, MIN_WINE_DESKTOP_WIDTH, NetworkPolicy,
    Permissions, RunnerKind, StorageKind, WineGraphicsBackend, WineLocale, WineVirtualDesktop,
};
use capsule::paths::{AppPaths, is_safe_relative, runtime_root};
use capsule::store::LibraryStore;
use capsule::{APP_NAME, backend};
use fs2::FileExt as Fs2FileExt;
use gtk::{Align, Orientation, SelectionMode, gio};
use uuid::Uuid;

pub fn install_style() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../assets/style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn build(application: &adw::Application) {
    let paths = match AppPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            show_startup_error(application, &error.to_string());
            return;
        }
    };
    if let Err(error) = paths.ensure() {
        show_startup_error(
            application,
            &format!("Cannot create Capsule data directory: {error}"),
        );
        return;
    }

    let store = LibraryStore::new(&paths.library_file);
    let (records, load_error) = match store.load_or_default() {
        Ok(library) => (library.capsules, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };

    let title = adw::WindowTitle::new(APP_NAME, "Library");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let back_button = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back to Library")
        .visible(false)
        .build();
    header.pack_start(&back_button);

    let doctor_button = gtk::Button::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Runtime status")
        .build();
    header.pack_start(&doctor_button);

    let add_button = gtk::Button::builder()
        .label("Add")
        .tooltip_text("Add a game or app")
        .build();
    add_button.add_css_class("suggested-action");

    let flow = gtk::FlowBox::builder()
        .column_spacing(14)
        .row_spacing(14)
        .homogeneous(false)
        .min_children_per_line(1)
        .max_children_per_line(1)
        .selection_mode(SelectionMode::None)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .hexpand(true)
        .vexpand(true)
        .build();
    let empty = empty_page();
    let library_scroll = library_page(&flow, &add_button, &empty);
    stack.add_named(&library_scroll, Some("library"));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&toolbar));

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title(APP_NAME)
        .default_width(1180)
        .default_height(760)
        .content(&overlay)
        .build();

    let state = Rc::new(UiState {
        window,
        overlay,
        title,
        stack,
        flow,
        library_scroll,
        empty,
        back_button,
        doctor_button: doctor_button.clone(),
        add_button: add_button.clone(),
        store,
        paths,
        records: RefCell::new(records),
        launching: RefCell::new(HashSet::new()),
    });
    state.refresh();

    {
        let state = Rc::clone(&state);
        add_button.connect_clicked(move |_| state.show_import_page());
    }
    {
        let state = Rc::clone(&state);
        doctor_button.connect_clicked(move |_| state.show_doctor());
    }
    {
        let state = Rc::clone(&state);
        state
            .back_button
            .clone()
            .connect_clicked(move |_| state.show_library());
    }

    state.window.present();
    state.begin_icon_backfill(state.records.borrow().clone());
    if let Some(error) = load_error {
        state.toast(&format!("Library was not loaded: {error}"));
    }
}

struct UiState {
    window: adw::ApplicationWindow,
    overlay: adw::ToastOverlay,
    title: adw::WindowTitle,
    stack: gtk::Stack,
    flow: gtk::FlowBox,
    library_scroll: gtk::ScrolledWindow,
    empty: gtk::Widget,
    back_button: gtk::Button,
    doctor_button: gtk::Button,
    add_button: gtk::Button,
    store: LibraryStore,
    paths: AppPaths,
    records: RefCell<Vec<CapsuleRecord>>,
    launching: RefCell<HashSet<Uuid>>,
}

#[derive(Clone)]
struct PortableSelection {
    source: backend::portable::PortableSource,
    inspection: backend::portable::PortableInspection,
    archive_password: Option<backend::portable::ArchivePassword>,
}

impl UiState {
    fn refresh(self: &Rc<Self>) {
        let library_visible = matches!(
            self.stack.visible_child_name().as_deref(),
            None | Some("library")
        );
        let scroll_value = library_visible.then(|| self.library_scroll.vadjustment().value());
        while let Some(child) = self.flow.first_child() {
            self.flow.remove(&child);
        }

        let records = self.records.borrow().clone();
        for record in &records {
            self.flow.insert(&self.card(record), -1);
        }

        if library_visible {
            self.show_library();
            let adjustment = self.library_scroll.vadjustment();
            gtk::glib::idle_add_local_once(move || {
                let maximum = (adjustment.upper() - adjustment.page_size()).max(0.0);
                adjustment.set_value(scroll_value.unwrap_or_default().min(maximum));
            });
        }
    }

    fn show_library(&self) {
        let count = self.records.borrow().len();
        self.title.set_title(APP_NAME);
        self.title.set_subtitle(&match count {
            0 => "No capsules yet".into(),
            1 => "1 capsule".into(),
            _ => format!("{count} capsules"),
        });
        self.back_button.set_visible(false);
        self.back_button.set_sensitive(true);
        self.doctor_button.set_visible(true);
        self.add_button.set_visible(true);
        self.flow.set_visible(count != 0);
        self.empty.set_visible(count == 0);
        self.stack.set_visible_child_name("library");
        if let Some(page) = self.stack.child_by_name("form") {
            self.stack.remove(&page);
        }
    }

    fn show_form(&self, page: &impl IsA<gtk::Widget>, title: &str, subtitle: &str) {
        if let Some(previous) = self.stack.child_by_name("form") {
            self.stack.remove(&previous);
        }
        self.stack.add_named(page, Some("form"));
        self.title.set_title(title);
        self.title.set_subtitle(subtitle);
        self.back_button.set_sensitive(true);
        self.back_button.set_visible(true);
        self.doctor_button.set_visible(false);
        self.add_button.set_visible(false);
        self.stack.set_visible_child_name("form");
    }

    fn card(self: &Rc<Self>, record: &CapsuleRecord) -> gtk::Widget {
        let card = gtk::Box::new(Orientation::Vertical, 0);
        card.add_css_class("card");
        card.add_css_class("capsule-card");

        let identity = gtk::Box::new(Orientation::Horizontal, 14);
        identity.set_valign(Align::Center);

        let icon_frame = gtk::Box::new(Orientation::Vertical, 0);
        icon_frame.add_css_class("capsule-icon-frame");
        icon_frame.set_halign(Align::Center);
        icon_frame.set_valign(Align::Center);
        icon_frame.append(&self.record_icon(record));
        identity.append(&icon_frame);

        let details_box = gtk::Box::new(Orientation::Vertical, 4);
        details_box.set_hexpand(true);
        details_box.set_valign(Align::Center);

        let name = gtk::Label::builder()
            .label(&record.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        name.add_css_class("capsule-name");
        name.set_tooltip_text(Some(&record.name));
        details_box.append(&name);

        let subtitle = gtk::Label::builder()
            .label(entrypoint_summary(record))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        subtitle.add_css_class("muted");
        subtitle.add_css_class("caption");
        details_box.append(&subtitle);

        let access = gtk::Label::builder()
            .label(access_summary(record))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        access.add_css_class("capsule-access");
        access.add_css_class("caption");
        details_box.append(&access);
        identity.append(&details_box);
        card.append(&identity);

        let actions = gtk::Box::new(Orientation::Horizontal, 8);
        actions.set_valign(Align::Center);
        let starting = self.launching.borrow().contains(&record.id);
        let running = capsule_image_is_locked(record);
        let busy = starting || running;
        let play = gtk::Button::builder()
            .label(if running {
                "Running"
            } else if starting {
                "Starting…"
            } else {
                "Start"
            })
            .width_request(110)
            .sensitive(!busy)
            .build();
        play.add_css_class("suggested-action");
        let settings = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text("Settings")
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Move to Trash")
            .sensitive(!busy)
            .build();

        actions.append(&play);
        actions.append(&settings);
        actions.append(&remove);
        identity.append(&actions);

        let id = record.id;
        {
            let state = Rc::clone(self);
            play.connect_clicked(move |_| state.launch(id));
        }
        {
            let state = Rc::clone(self);
            settings.connect_clicked(move |_| state.show_settings(id));
        }
        {
            let state = Rc::clone(self);
            remove.connect_clicked(move |_| state.confirm_remove(id));
        }

        card.upcast()
    }

    fn record_icon(&self, record: &CapsuleRecord) -> gtk::Widget {
        let path = self.paths.icon_path(record.id);
        if path.is_file() {
            let picture = gtk::Picture::for_filename(path);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_size_request(72, 72);
            picture.set_alternative_text(Some(&format!("{} icon", record.name)));
            picture.add_css_class("capsule-game-icon");
            picture.upcast()
        } else {
            let icon = gtk::Image::from_icon_name(match record.runner {
                RunnerKind::Wine => "applications-games-symbolic",
                RunnerKind::Native => "application-x-executable-symbolic",
            });
            icon.set_pixel_size(38);
            icon.add_css_class("dim-label");
            icon.upcast()
        }
    }

    fn begin_icon_backfill(self: &Rc<Self>, records: Vec<CapsuleRecord>) {
        for record in &records {
            if self.paths.icon_path(record.id).is_file() {
                let _ = std::fs::remove_file(self.paths.legacy_icon_path(record.id));
            }
        }
        let pending: Vec<_> = records
            .into_iter()
            .filter(|record| !self.paths.icon_path(record.id).is_file())
            .collect();
        if pending.is_empty() {
            return;
        }
        let Ok(capsule_executable) = std::env::current_exe() else {
            return;
        };
        let Ok(runtime_root) = runtime_root() else {
            return;
        };
        let paths = self.paths.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let capabilities = backend::capabilities::Capabilities::detect();
            let mut changed = false;
            for record in pending {
                let destination = paths.icon_path(record.id);
                match backend::icon::cache_record_icon(
                    &record,
                    &destination,
                    &runtime_root,
                    &capsule_executable,
                    &capabilities,
                ) {
                    Ok(()) => changed = true,
                    Err(error) => {
                        eprintln!("Capsule icon unavailable for {}: {error}", record.name)
                    }
                }
            }
            let _ = sender.send(changed);
        });

        let state = Rc::clone(self);
        gtk::glib::timeout_add_local(Duration::from_millis(150), move || {
            match receiver.try_recv() {
                Ok(true) => {
                    state.refresh();
                    gtk::glib::ControlFlow::Break
                }
                Ok(false) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    gtk::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            }
        });
    }

    fn show_import_page(self: &Rc<Self>) {
        let page = adw::PreferencesPage::new();

        let source_group = adw::PreferencesGroup::builder()
            .title("Source")
            .description(
                "Choose a folder or archive. Multipart 7z, ZIP, and RAR sets are supported.",
            )
            .build();
        let name_row = adw::EntryRow::builder().title("Name").build();

        let source_row = adw::ActionRow::builder()
            .title("Game files")
            .subtitle("Choose a folder or archive")
            .subtitle_lines(3)
            .build();
        let choose_folder = gtk::Button::with_label("Folder…");
        let choose_archive = gtk::Button::with_label("Archive…");
        choose_folder.set_valign(Align::Center);
        choose_archive.set_valign(Align::Center);
        let source_buttons = gtk::Box::new(Orientation::Horizontal, 6);
        source_buttons.append(&choose_folder);
        source_buttons.append(&choose_archive);
        source_row.add_suffix(&source_buttons);
        source_group.add(&source_row);
        let password_row = adw::PasswordEntryRow::builder()
            .title("Archive password")
            .visible(false)
            .build();
        password_row.set_max_length(1024);
        let inspect_password = gtk::Button::with_label("Inspect");
        inspect_password.set_valign(Align::Center);
        password_row.add_suffix(&inspect_password);
        source_group.add(&password_row);
        source_group.add(&name_row);

        let executable_names = gtk::StringList::new(&[]);
        let executable_row = adw::ComboRow::builder()
            .title("Program to start")
            .model(&executable_names)
            .use_subtitle(true)
            .subtitle_lines(0)
            .sensitive(false)
            .build();
        source_group.add(&executable_row);
        page.add(&source_group);

        let isolation_group = adw::PreferencesGroup::builder()
            .title("Access")
            .description("Start restricted, then enable only what the game needs.")
            .build();

        let profile_names = gtk::StringList::new(&["Locked", "Offline game", "Online game"]);
        let profile_row = adw::ComboRow::builder()
            .title("Preset")
            .subtitle(profile_description(1))
            .subtitle_lines(2)
            .model(&profile_names)
            .selected(1)
            .build();
        profile_row.connect_selected_notify(|row| {
            row.set_subtitle(profile_description(row.selected()));
        });
        isolation_group.add(&profile_row);

        let audio_group = adw::ExpanderRow::builder()
            .title("Audio")
            .activatable(false)
            .expanded(true)
            .build();
        audio_group.add_css_class("static-group");
        let playback_row = adw::SwitchRow::builder()
            .title("Playback")
            .active(false)
            .build();
        let microphone_row = adw::SwitchRow::builder()
            .title("Microphone")
            .active(false)
            .build();
        bind_audio_rows(&playback_row, &microphone_row);
        audio_group.add_row(&playback_row);
        audio_group.add_row(&microphone_row);
        isolation_group.add(&audio_group);

        let warning = adw::ActionRow::builder()
            .title("Container isolation")
            .subtitle("The app cannot see your files, but it shares the Linux kernel and graphics driver.")
            .subtitle_lines(3)
            .build();
        warning.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        isolation_group.add(&warning);
        page.add(&isolation_group);

        let footer = gtk::Box::new(Orientation::Horizontal, 10);
        footer.set_halign(Align::End);
        footer.set_margin_top(12);
        footer.set_margin_bottom(18);
        footer.set_margin_start(24);
        footer.set_margin_end(24);
        let cancel = gtk::Button::with_label("Cancel");
        let create = gtk::Button::with_label("Add to Library");
        create.set_sensitive(false);
        create.add_css_class("suggested-action");
        footer.append(&cancel);
        footer.append(&create);

        let body = gtk::Box::new(Orientation::Vertical, 0);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&page)
            .build();
        body.append(&scroll);
        body.append(&footer);
        self.show_form(&body, "Add to Library", "Game or app");

        let selection: Rc<RefCell<Option<PortableSelection>>> = Rc::new(RefCell::new(None));
        let automatic_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let pending_archive: Rc<RefCell<Option<backend::portable::PortableSource>>> =
            Rc::new(RefCell::new(None));
        {
            let window = self.window.clone();
            let selection = Rc::clone(&selection);
            let automatic_name = Rc::clone(&automatic_name);
            let source_row = source_row.clone();
            let name_row = name_row.clone();
            let executable_row = executable_row.clone();
            let executable_names = executable_names.clone();
            let choose_folder_button = choose_folder.clone();
            let choose_archive_button = choose_archive.clone();
            let create = create.clone();
            let password_row = password_row.clone();
            let inspect_password = inspect_password.clone();
            let pending_archive = Rc::clone(&pending_archive);
            choose_folder.connect_clicked(move |_| {
                let file_dialog = gtk::FileDialog::builder()
                    .title("Choose a portable game or app folder")
                    .modal(true)
                    .build();
                let selection = Rc::clone(&selection);
                let automatic_name = Rc::clone(&automatic_name);
                let source_row = source_row.clone();
                let name_row = name_row.clone();
                let executable_row = executable_row.clone();
                let executable_names = executable_names.clone();
                let choose_folder = choose_folder_button.clone();
                let choose_archive = choose_archive_button.clone();
                let create = create.clone();
                let password_row = password_row.clone();
                let inspect_password = inspect_password.clone();
                let pending_archive = Rc::clone(&pending_archive);
                file_dialog.select_folder(
                    Some(&window),
                    None::<&gio::Cancellable>,
                    move |result| {
                        if let Ok(file) = result
                            && let Some(path) = file.path()
                        {
                            begin_portable_inspection(
                                backend::portable::PortableSource::Directory(path),
                                &selection,
                                &automatic_name,
                                &source_row,
                                &name_row,
                                &executable_row,
                                &executable_names,
                                &choose_folder,
                                &choose_archive,
                                &create,
                                &password_row,
                                &inspect_password,
                                &pending_archive,
                                None,
                            );
                        }
                    },
                );
            });
        }
        {
            let window = self.window.clone();
            let selection = Rc::clone(&selection);
            let automatic_name = Rc::clone(&automatic_name);
            let source_row = source_row.clone();
            let name_row = name_row.clone();
            let executable_row = executable_row.clone();
            let executable_names = executable_names.clone();
            let choose_folder_button = choose_folder.clone();
            let choose_archive_button = choose_archive.clone();
            let create = create.clone();
            let password_row = password_row.clone();
            let inspect_password = inspect_password.clone();
            let pending_archive = Rc::clone(&pending_archive);
            choose_archive.connect_clicked(move |_| {
                let archive_filter = gtk::FileFilter::new();
                archive_filter.set_name(Some("Archives"));
                archive_filter.add_mime_type("application/zip");
                archive_filter.add_mime_type("application/x-7z-compressed");
                archive_filter.add_mime_type("application/vnd.rar");
                archive_filter.add_mime_type("application/x-rar");
                archive_filter.add_mime_type("application/x-tar");
                archive_filter.add_mime_type("application/gzip");
                archive_filter.add_mime_type("application/x-bzip2");
                archive_filter.add_mime_type("application/x-xz");
                archive_filter.add_mime_type("application/zstd");
                archive_filter.add_mime_type("application/x-cab");
                archive_filter.add_mime_type("application/x-iso9660-image");
                for pattern in [
                    "*.zip", "*.ZIP", "*.zipx", "*.ZIPX", "*.7z", "*.7Z", "*.rar", "*.RAR",
                    "*.r??", "*.R??", "*.z??", "*.Z??", "*.001", "*.cab", "*.CAB", "*.arj",
                    "*.ARJ", "*.lzh", "*.LZH", "*.lha", "*.LHA", "*.tar", "*.TAR", "*.tgz",
                    "*.TGZ", "*.tbz", "*.TBZ", "*.tbz2", "*.TBZ2", "*.txz", "*.TXZ", "*.tzst",
                    "*.TZST", "*.gz", "*.GZ", "*.bz2", "*.BZ2", "*.xz", "*.XZ", "*.zst", "*.ZST",
                    "*.lzma", "*.LZMA", "*.wim", "*.WIM", "*.swm", "*.SWM", "*.iso", "*.ISO",
                    "*.xar", "*.XAR", "*.cpio", "*.CPIO",
                ] {
                    archive_filter.add_pattern(pattern);
                }
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&archive_filter);
                let all_files = gtk::FileFilter::new();
                all_files.set_name(Some("All files"));
                all_files.add_pattern("*");
                filters.append(&all_files);
                let file_dialog = gtk::FileDialog::builder()
                    .title("Choose a portable game or app archive")
                    .modal(true)
                    .filters(&filters)
                    .default_filter(&archive_filter)
                    .build();
                let selection = Rc::clone(&selection);
                let automatic_name = Rc::clone(&automatic_name);
                let source_row = source_row.clone();
                let name_row = name_row.clone();
                let executable_row = executable_row.clone();
                let executable_names = executable_names.clone();
                let choose_folder = choose_folder_button.clone();
                let choose_archive = choose_archive_button.clone();
                let create = create.clone();
                let password_row = password_row.clone();
                let inspect_password = inspect_password.clone();
                let pending_archive = Rc::clone(&pending_archive);
                file_dialog.open(Some(&window), None::<&gio::Cancellable>, move |result| {
                    if let Ok(file) = result
                        && let Some(path) = file.path()
                    {
                        begin_portable_inspection(
                            backend::portable::portable_archive_source(path),
                            &selection,
                            &automatic_name,
                            &source_row,
                            &name_row,
                            &executable_row,
                            &executable_names,
                            &choose_folder,
                            &choose_archive,
                            &create,
                            &password_row,
                            &inspect_password,
                            &pending_archive,
                            None,
                        );
                    }
                });
            });
        }
        {
            let selection = Rc::clone(&selection);
            let automatic_name = Rc::clone(&automatic_name);
            let source_row = source_row.clone();
            let name_row = name_row.clone();
            let executable_row = executable_row.clone();
            let executable_names = executable_names.clone();
            let choose_folder = choose_folder.clone();
            let choose_archive = choose_archive.clone();
            let create = create.clone();
            let password_row = password_row.clone();
            let inspect_password_button = inspect_password.clone();
            let pending_archive = Rc::clone(&pending_archive);
            inspect_password.connect_clicked(move |_| {
                let password = password_row.text().to_string();
                if password.is_empty() {
                    source_row.set_subtitle("Enter the archive password first");
                    return;
                }
                let Some(source) = pending_archive.borrow().clone() else {
                    source_row.set_subtitle("Choose a password-protected archive first");
                    return;
                };
                begin_portable_inspection(
                    source,
                    &selection,
                    &automatic_name,
                    &source_row,
                    &name_row,
                    &executable_row,
                    &executable_names,
                    &choose_folder,
                    &choose_archive,
                    &create,
                    &password_row,
                    &inspect_password_button,
                    &pending_archive,
                    Some(backend::portable::ArchivePassword::new(password)),
                );
            });
        }
        {
            let state = Rc::clone(self);
            cancel.connect_clicked(move |_| state.show_library());
        }
        {
            let state = Rc::clone(self);
            let page = page.clone();
            let selection = Rc::clone(&selection);
            let create_button = create.clone();
            let cancel_button = cancel.clone();
            create.connect_clicked(move |_| {
                let name = name_row.text().trim().to_owned();
                let Some(selection) = selection.borrow().clone() else {
                    state.toast("Choose a game folder or archive first");
                    return;
                };
                let Some((entrypoint, runner)) = selection
                    .inspection
                    .candidate(executable_row.selected() as usize)
                    .map(|(path, runner)| (path.to_path_buf(), runner))
                else {
                    state.toast("Choose the program to start");
                    return;
                };
                if name.is_empty() {
                    state.toast("Capsule name cannot be empty");
                    return;
                }

                let storage = StorageKind::Image {
                    path: state.paths.capsule_path(&name),
                };

                let mut record = CapsuleRecord::new(name, storage, entrypoint.clone(), runner);
                record.permissions = permissions_for_selection(
                    profile_row.selected(),
                    playback_row.is_active(),
                    microphone_row.is_active(),
                );

                let StorageKind::Image { path: image_path } = &record.storage else {
                    unreachable!();
                };
                if image_path.exists() {
                    state.toast("A capsule with that file name already exists");
                    return;
                }
                let Ok(runtime_root) = runtime_root() else {
                    state.toast("XDG_RUNTIME_DIR is unavailable; import was blocked");
                    return;
                };
                let image_size_mib = backend::portable::recommended_image_size_mib(
                    selection.inspection.uncompressed_bytes,
                );
                page.set_sensitive(false);
                create_button.set_sensitive(false);
                create_button.set_label("Creating…");
                cancel_button.set_sensitive(false);
                state.back_button.set_sensitive(false);
                state.window.set_deletable(false);
                let request = backend::portable::PortableImportRequest {
                    id: record.id,
                    name: record.name.clone(),
                    source: selection.source,
                    archive_password: selection.archive_password,
                    image_path: image_path.clone(),
                    image_size_mib,
                    runtime_root,
                    limits: backend::portable::ImportLimits::default(),
                };
                let capabilities = backend::capabilities::Capabilities::detect();
                let (sender, receiver) = std::sync::mpsc::channel();
                let selected_entrypoint = entrypoint.clone();
                let selected_runner = runner;
                std::thread::spawn(move || {
                    let result = backend::portable::import_portable_game(&request, &capabilities)
                        .map_err(|error| error.to_string())
                        .and_then(|result| {
                            if result
                                .inspection
                                .executable_candidates
                                .iter()
                                .zip(&result.inspection.candidate_runners)
                                .any(|(path, runner)| {
                                    path == &selected_entrypoint && *runner == selected_runner
                                })
                            {
                                Ok(())
                            } else {
                                let _ = std::fs::remove_file(&request.image_path);
                                Err("The selected executable changed during import".into())
                            }
                        });
                    let _ = sender.send(result);
                });

                let state = Rc::clone(&state);
                let page = page.clone();
                let create = create_button.clone();
                let cancel = cancel_button.clone();
                gtk::glib::timeout_add_local(Duration::from_millis(150), move || {
                    match receiver.try_recv() {
                        Ok(Ok(())) => {
                            state.finish_import(record.clone());
                            gtk::glib::ControlFlow::Break
                        }
                        Ok(Err(error)) => {
                            page.set_sensitive(true);
                            create.set_sensitive(true);
                            create.set_label("Add to Library");
                            cancel.set_sensitive(true);
                            state.back_button.set_sensitive(true);
                            state.window.set_deletable(true);
                            state.toast(&format!("Capsule import failed: {error}"));
                            gtk::glib::ControlFlow::Break
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            gtk::glib::ControlFlow::Continue
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            page.set_sensitive(true);
                            create.set_sensitive(true);
                            create.set_label("Add to Library");
                            cancel.set_sensitive(true);
                            state.back_button.set_sensitive(true);
                            state.window.set_deletable(true);
                            state.toast("Importer stopped without returning a result");
                            gtk::glib::ControlFlow::Break
                        }
                    }
                });
            });
        }
    }

    fn launch(self: &Rc<Self>, id: Uuid) {
        let Some(record) = self
            .records
            .borrow()
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            self.toast("Capsule is no longer in the library");
            return;
        };
        if self.launching.borrow().contains(&id) || capsule_image_is_locked(&record) {
            self.toast(&format!("{} is already starting or running", record.name));
            return;
        }

        let executable = match supervisor_executable() {
            Ok(executable) => executable,
            Err(error) => {
                self.toast(&format!("Could not resolve Capsule supervisor: {error}"));
                return;
            }
        };
        let mut supervisor = Command::new(executable);
        supervisor.arg("--supervise").arg(record.id.to_string());
        let niri_windows_before = niri_gamescope_window_ids();
        self.launching.borrow_mut().insert(id);
        self.refresh();

        match supervisor.spawn() {
            Ok(mut child) => {
                self.toast(&format!("Starting {}", record.name));
                focus_new_niri_gamescope_window(niri_windows_before, record.name.clone());
                let state = Rc::clone(self);
                gtk::glib::timeout_add_local_once(Duration::from_secs(1), move || {
                    state.refresh();
                });
                let state = Rc::clone(self);
                let name = record.name;
                gtk::glib::timeout_add_local(Duration::from_millis(500), move || {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            state.launching.borrow_mut().remove(&id);
                            state.refresh();
                            if status.success() {
                                state.toast(&format!("{name} stopped"));
                            } else {
                                state.toast(&format!("{name} was blocked or exited with an error"));
                            }
                            gtk::glib::ControlFlow::Break
                        }
                        Ok(None) => gtk::glib::ControlFlow::Continue,
                        Err(error) => {
                            state.launching.borrow_mut().remove(&id);
                            state.refresh();
                            state.toast(&format!("Could not monitor {name}: {error}"));
                            gtk::glib::ControlFlow::Break
                        }
                    }
                });
            }
            Err(error) => {
                self.launching.borrow_mut().remove(&id);
                self.refresh();
                self.toast(&format!("Could not start supervisor: {error}"));
            }
        }
    }

    fn launch_steam_tool(
        self: &Rc<Self>,
        id: Uuid,
        action: &'static str,
        starting_message: &'static str,
        success_message: &'static str,
        install_button: &gtk::Button,
        open_button: &gtk::Button,
    ) {
        let Some(record) = self
            .records
            .borrow()
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            self.toast("Capsule is no longer in the library");
            return;
        };
        if record.runner != RunnerKind::Wine {
            self.toast("Steam can be installed only in Wine capsules");
            return;
        }
        if self.launching.borrow().contains(&id) || capsule_image_is_locked(&record) {
            self.toast(&format!("{} is already starting or running", record.name));
            return;
        }
        let executable = match supervisor_executable() {
            Ok(executable) => executable,
            Err(error) => {
                self.toast(&format!("Could not resolve Capsule supervisor: {error}"));
                return;
            }
        };

        let mut supervisor = Command::new(executable);
        supervisor.arg(action).arg(id.to_string());
        let niri_windows_before = niri_gamescope_window_ids();
        install_button.set_sensitive(false);
        open_button.set_sensitive(false);
        self.launching.borrow_mut().insert(id);
        self.refresh();

        match supervisor.spawn() {
            Ok(mut child) => {
                self.toast(starting_message);
                focus_new_niri_gamescope_window(niri_windows_before, record.name.clone());
                let state = Rc::clone(self);
                let install = install_button.clone();
                let open = open_button.clone();
                let name = record.name;
                gtk::glib::timeout_add_local(Duration::from_millis(500), move || {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            state.launching.borrow_mut().remove(&id);
                            state.refresh();
                            install.set_sensitive(true);
                            open.set_sensitive(true);
                            if status.success() {
                                state.toast(success_message);
                            } else {
                                state.toast(&format!(
                                    "Steam action for {name} was blocked or exited with an error"
                                ));
                            }
                            gtk::glib::ControlFlow::Break
                        }
                        Ok(None) => gtk::glib::ControlFlow::Continue,
                        Err(error) => {
                            state.launching.borrow_mut().remove(&id);
                            state.refresh();
                            install.set_sensitive(true);
                            open.set_sensitive(true);
                            state.toast(&format!("Could not monitor Steam: {error}"));
                            gtk::glib::ControlFlow::Break
                        }
                    }
                });
            }
            Err(error) => {
                self.launching.borrow_mut().remove(&id);
                self.refresh();
                install_button.set_sensitive(true);
                open_button.set_sensitive(true);
                self.toast(&format!("Could not start Steam action: {error}"));
            }
        }
    }

    fn finish_import(self: &Rc<Self>, record: CapsuleRecord) {
        match self.store.create(record) {
            Ok(created) => {
                self.records.borrow_mut().push(created.clone());
                self.refresh();
                self.begin_icon_backfill(vec![created]);
                self.window.set_deletable(true);
                self.show_library();
                self.toast("Capsule added");
            }
            Err(error) => {
                self.window.set_deletable(true);
                self.back_button.set_sensitive(true);
                self.toast(&format!("Could not add capsule to library: {error}"));
            }
        }
    }

    fn show_settings(self: &Rc<Self>, id: Uuid) {
        let Some(record) = self
            .records
            .borrow()
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            return;
        };

        let page = adw::PreferencesPage::new();
        let identity = adw::PreferencesGroup::builder().title("Capsule").build();
        let name_row = adw::EntryRow::builder()
            .title("Name")
            .text(&record.name)
            .build();
        let entry_row = adw::EntryRow::builder()
            .title("Program")
            .text(record.entrypoint.to_string_lossy())
            .build();
        identity.add(&name_row);
        identity.add(&entry_row);
        let storage = adw::ActionRow::builder()
            .title("Storage")
            .subtitle(record.storage.path().to_string_lossy())
            .build();
        identity.add(&storage);
        page.add(&identity);

        let wine_display = adw::PreferencesGroup::builder()
            .title("Wine compatibility")
            .description("Workarounds for older Windows apps.")
            .build();
        wine_display.set_visible(record.runner == RunnerKind::Wine);
        let graphics_names = gtk::StringList::new(&["DXVK (Vulkan)", "WineD3D (compatibility)"]);
        let graphics_row = adw::ComboRow::builder()
            .title("Graphics")
            .model(&graphics_names)
            .selected(match record.wine_graphics_backend {
                WineGraphicsBackend::Dxvk => 0,
                WineGraphicsBackend::WineD3d => 1,
            })
            .build();
        wine_display.add(&graphics_row);
        let locale_choices = Rc::new(wine_locale_choices().to_vec());
        let selected_locale = locale_choices
            .iter()
            .position(|choice| choice.locale == record.wine_locale)
            .or_else(|| {
                locale_choices
                    .iter()
                    .position(|choice| choice.locale.is_default())
            })
            .unwrap_or(0);
        let locale_selection =
            Rc::new(RefCell::new(locale_choices[selected_locale].locale.clone()));
        let locale_value = gtk::Label::builder()
            .label(&locale_choices[selected_locale].label)
            .halign(Align::End)
            .xalign(1.0)
            .single_line_mode(true)
            .max_width_chars(42)
            .build();
        let locale_row = adw::ExpanderRow::builder()
            .title("Language for older apps")
            .expanded(false)
            .build();
        locale_row.add_suffix(&locale_value);

        let locale_search = gtk::SearchEntry::builder()
            .placeholder_text("Search languages and countries")
            .build();
        let locale_list = gtk::ListBox::builder()
            .selection_mode(SelectionMode::None)
            .build();
        locale_list.add_css_class("boxed-list");
        let locale_rows = Rc::new(RefCell::new(Vec::<(String, adw::ActionRow)>::new()));
        let locale_checks = Rc::new(RefCell::new(Vec::<(WineLocale, gtk::Image)>::new()));
        for choice in locale_choices.iter() {
            let row = adw::ActionRow::builder()
                .title(&choice.label)
                .title_lines(2)
                .activatable(true)
                .build();
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_visible(choice.locale == *locale_selection.borrow());
            row.add_suffix(&check);
            locale_list.append(&row);
            locale_rows
                .borrow_mut()
                .push((choice.label.to_lowercase(), row.clone()));
            locale_checks
                .borrow_mut()
                .push((choice.locale.clone(), check));

            let locale_selection = Rc::clone(&locale_selection);
            let locale_checks = Rc::clone(&locale_checks);
            let locale_value = locale_value.clone();
            let locale_row = locale_row.clone();
            let locale_search = locale_search.clone();
            let chosen_locale = choice.locale.clone();
            let chosen_label = choice.label.clone();
            row.connect_activated(move |_| {
                *locale_selection.borrow_mut() = chosen_locale.clone();
                locale_value.set_label(&chosen_label);
                for (locale, check) in locale_checks.borrow().iter() {
                    check.set_visible(*locale == chosen_locale);
                }
                locale_search.set_text("");
                locale_row.set_expanded(false);
            });
        }

        let locale_empty = adw::StatusPage::builder()
            .icon_name("system-search-symbolic")
            .title("No languages found")
            .build();
        let locale_results = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let locale_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(280)
            .max_content_height(280)
            .propagate_natural_height(true)
            .child(&locale_list)
            .build();
        locale_results.add_named(&locale_scroll, Some("list"));
        locale_results.add_named(&locale_empty, Some("empty"));
        locale_results.set_visible_child_name("list");
        {
            let locale_rows = Rc::clone(&locale_rows);
            let locale_results = locale_results.clone();
            locale_search.connect_search_changed(move |entry| {
                let query = entry.text().trim().to_lowercase();
                let mut visible = 0;
                for (label, row) in locale_rows.borrow().iter() {
                    let matches = query.is_empty() || label.contains(&query);
                    row.set_visible(matches);
                    visible += usize::from(matches);
                }
                locale_results.set_visible_child_name(if visible == 0 { "empty" } else { "list" });
            });
        }
        {
            let locale_search = locale_search.clone();
            locale_row.connect_expanded_notify(move |row| {
                if row.is_expanded() {
                    locale_search.grab_focus();
                }
            });
        }
        let locale_picker = gtk::Box::new(Orientation::Vertical, 10);
        locale_picker.set_margin_top(10);
        locale_picker.set_margin_bottom(12);
        locale_picker.set_margin_start(12);
        locale_picker.set_margin_end(12);
        locale_picker.append(&locale_search);
        locale_picker.append(&locale_results);
        locale_row.add_row(&locale_picker);
        wine_display.add(&locale_row);
        let virtual_desktop_enabled = record.wine_virtual_desktop.is_some();
        let virtual_desktop_row = adw::ExpanderRow::builder()
            .title("Use a virtual desktop")
            .show_enable_switch(true)
            .enable_expansion(virtual_desktop_enabled)
            .expanded(virtual_desktop_enabled)
            .build();

        let desktop_size = wine_desktop_size_for_settings(record.wine_virtual_desktop);
        let desktop_width_row = adw::SpinRow::with_range(
            MIN_WINE_DESKTOP_WIDTH as f64,
            MAX_WINE_DESKTOP_DIMENSION as f64,
            16.0,
        );
        desktop_width_row.set_title("Desktop width");
        desktop_width_row.set_value(desktop_size.width as f64);
        virtual_desktop_row.add_row(&desktop_width_row);

        let desktop_height_row = adw::SpinRow::with_range(
            MIN_WINE_DESKTOP_HEIGHT as f64,
            MAX_WINE_DESKTOP_DIMENSION as f64,
            16.0,
        );
        desktop_height_row.set_title("Desktop height");
        desktop_height_row.set_value(desktop_size.height as f64);
        virtual_desktop_row.add_row(&desktop_height_row);
        wine_display.add(&virtual_desktop_row);

        page.add(&wine_display);

        let steam = adw::PreferencesGroup::builder()
            .title("Steam inside this capsule")
            .description(
                "Installs the Windows Steam client into this Wine prefix. Its login and account session are visible to applications in this capsule only.",
            )
            .build();
        steam.set_visible(record.runner == RunnerKind::Wine);
        let steam_start_row = adw::SwitchRow::builder()
            .title("Start Steam with this game")
            .subtitle("Steam starts silently in the same contained Wine session before the game.")
            .active(record.wine_steam)
            .build();
        steam.add(&steam_start_row);
        let steam_install_row = adw::ActionRow::builder()
            .title("Install or repair Steam")
            .subtitle("Downloads Valve's current Windows installer and opens its normal setup and login flow.")
            .subtitle_lines(3)
            .build();
        let steam_install_button = gtk::Button::with_label("Install…");
        steam_install_button.set_valign(Align::Center);
        steam_install_row.add_suffix(&steam_install_button);
        steam.add(&steam_install_row);
        let steam_open_row = adw::ActionRow::builder()
            .title("Open Steam")
            .subtitle("Sign in, update Steam, or manage the account stored in this capsule.")
            .subtitle_lines(2)
            .build();
        let steam_open_button = gtk::Button::with_label("Open…");
        steam_open_button.set_valign(Align::Center);
        steam_open_row.add_suffix(&steam_open_button);
        steam.add(&steam_open_row);
        page.add(&steam);

        virtual_desktop_row.connect_enable_expansion_notify(|row| {
            row.set_expanded(row.enables_expansion());
        });

        {
            let state = Rc::clone(self);
            let install = steam_install_button.clone();
            let open = steam_open_button.clone();
            steam_install_button.connect_clicked(move |_| {
                state.launch_steam_tool(
                    id,
                    "--install-steam",
                    "Downloading and opening the Steam installer",
                    "Steam installer closed",
                    &install,
                    &open,
                );
            });
        }
        {
            let state = Rc::clone(self);
            let install = steam_install_button.clone();
            let open = steam_open_button.clone();
            steam_open_button.connect_clicked(move |_| {
                state.launch_steam_tool(
                    id,
                    "--open-steam",
                    "Opening Steam",
                    "Steam closed",
                    &install,
                    &open,
                );
            });
        }

        let access = adw::PreferencesGroup::builder()
            .title("Access")
            .description("Enabled options give the app access to host resources.")
            .build();
        let global_network_enabled = matches!(
            record.permissions.network,
            NetworkPolicy::InternetOnly | NetworkPolicy::Lan | NetworkPolicy::Custom { .. }
        );
        let local_network_enabled = matches!(
            record.permissions.network,
            NetworkPolicy::LanOnly | NetworkPolicy::Lan
        );
        let network_group = adw::ExpanderRow::builder()
            .title("Network")
            .activatable(false)
            .expanded(true)
            .build();
        network_group.add_css_class("static-group");
        let global_network_row = adw::SwitchRow::builder()
            .title("Global network")
            .active(global_network_enabled)
            .build();
        let local_network_row = adw::SwitchRow::builder()
            .title("Local network")
            .active(local_network_enabled)
            .build();
        network_group.add_row(&global_network_row);
        network_group.add_row(&local_network_row);
        access.add(&network_group);

        let audio_group = adw::ExpanderRow::builder()
            .title("Audio")
            .activatable(false)
            .expanded(true)
            .build();
        audio_group.add_css_class("static-group");
        let playback_row = adw::SwitchRow::builder()
            .title("Playback")
            .active(!matches!(record.permissions.audio, AudioPolicy::Off))
            .build();
        let microphone_row = adw::SwitchRow::builder()
            .title("Microphone")
            .active(matches!(
                record.permissions.audio,
                AudioPolicy::PlaybackAndMicrophone
            ))
            .build();
        bind_audio_rows(&playback_row, &microphone_row);
        audio_group.add_row(&playback_row);
        audio_group.add_row(&microphone_row);
        access.add(&audio_group);

        let gpu_row = adw::SwitchRow::builder()
            .title("Direct GPU rendering")
            .active(record.permissions.gpu)
            .build();
        access.add(&gpu_row);
        let clipboard_row = adw::SwitchRow::builder()
            .title("Clipboard")
            .active(record.permissions.clipboard)
            .build();
        access.add(&clipboard_row);
        page.add(&access);

        let limits = adw::PreferencesGroup::builder()
            .title("Resource limits")
            .description("A value of zero means no explicit limit.")
            .build();
        let memory_row = adw::SpinRow::with_range(0.0, 131_072.0, 256.0);
        memory_row.set_title("Memory (MiB)");
        memory_row.set_value(record.permissions.memory_limit_mib.unwrap_or(0) as f64);
        limits.add(&memory_row);
        let process_row = adw::SpinRow::with_range(0.0, 8_192.0, 64.0);
        process_row.set_title("Maximum processes");
        process_row.set_value(record.permissions.process_limit.unwrap_or(0) as f64);
        limits.add(&process_row);
        page.add(&limits);

        let footer = gtk::Box::new(Orientation::Horizontal, 10);
        footer.set_halign(Align::End);
        footer.set_margin_top(12);
        footer.set_margin_bottom(18);
        footer.set_margin_start(24);
        footer.set_margin_end(24);
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
        footer.append(&cancel);
        footer.append(&save);

        let body = gtk::Box::new(Orientation::Vertical, 0);
        body.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&page)
                .build(),
        );
        body.append(&footer);
        self.show_form(&body, &record.name, "Settings");

        {
            let state = Rc::clone(self);
            cancel.connect_clicked(move |_| state.show_library());
        }
        {
            let state = Rc::clone(self);
            let locale_selection = Rc::clone(&locale_selection);
            save.connect_clicked(move |_| {
                let name = name_row.text().trim().to_owned();
                let entrypoint = PathBuf::from(entry_row.text().trim());
                if name.is_empty() || !is_safe_relative(&entrypoint) {
                    state.toast("Name and capsule-relative executable are required");
                    return;
                }
                let mut updated = record.clone();
                updated.name = name;
                updated.entrypoint = entrypoint;
                updated.permissions.network = match (
                    global_network_row.is_active(),
                    local_network_row.is_active(),
                ) {
                    (true, true) => NetworkPolicy::Lan,
                    (true, false) => NetworkPolicy::InternetOnly,
                    (false, true) => NetworkPolicy::LanOnly,
                    (false, false) => NetworkPolicy::Off,
                };
                updated.permissions.audio =
                    audio_policy_from_rows(playback_row.is_active(), microphone_row.is_active());
                updated.permissions.gpu = gpu_row.is_active();
                updated.permissions.controllers = false;
                updated.permissions.clipboard = clipboard_row.is_active();
                updated.permissions.memory_limit_mib = nonzero_u64(memory_row.value());
                updated.permissions.process_limit = nonzero_u32(process_row.value());
                updated.permissions.isolation_profile = infer_profile(&updated.permissions);
                if updated.runner == RunnerKind::Wine {
                    updated.wine_virtual_desktop =
                        virtual_desktop_row
                            .enables_expansion()
                            .then(|| WineVirtualDesktop {
                                width: desktop_width_row.value().round() as u32,
                                height: desktop_height_row.value().round() as u32,
                            });
                    updated.wine_locale = locale_selection.borrow().clone();
                    updated.wine_graphics_backend = match graphics_row.selected() {
                        1 => WineGraphicsBackend::WineD3d,
                        _ => WineGraphicsBackend::Dxvk,
                    };
                    updated.wine_steam = steam_start_row.is_active();
                } else {
                    updated.wine_virtual_desktop = None;
                    updated.wine_steam = false;
                }

                match state.store.update(updated.clone()) {
                    Ok(()) => {
                        if let Some(target) = state
                            .records
                            .borrow_mut()
                            .iter_mut()
                            .find(|candidate| candidate.id == id)
                        {
                            *target = updated;
                        }
                        state.refresh();
                        state.show_library();
                        state.toast("Capsule settings saved");
                    }
                    Err(error) => state.toast(&format!("Could not save settings: {error}")),
                }
            });
        }
    }

    fn confirm_remove(self: &Rc<Self>, id: Uuid) {
        let Some(record) = self
            .records
            .borrow()
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            return;
        };
        let is_image = record.storage.is_image();
        let body = if is_image {
            "The capsule file will be moved to Trash. This removes the contained game, Wine prefix, saves and capsule-specific caches."
        } else {
            "This development entry will be removed from the library. Its source directory will not be deleted."
        };
        let dialog = adw::AlertDialog::new(Some(&format!("Remove {}?", record.name)), Some(body));
        dialog.add_responses(&[
            ("cancel", "Cancel"),
            (
                "remove",
                if is_image {
                    "Move to Trash"
                } else {
                    "Unregister"
                },
            ),
        ]);
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        let state = Rc::clone(self);
        dialog.connect_response(Some("remove"), move |_, response| {
            if response == "remove" {
                state.remove(id);
            }
        });
        dialog.present(Some(&self.window));
    }

    fn remove(self: &Rc<Self>, id: Uuid) {
        let Some(record) = self
            .records
            .borrow()
            .iter()
            .find(|record| record.id == id)
            .cloned()
        else {
            return;
        };
        let _image_lock = if let StorageKind::Image { path } = &record.storage {
            let locked_file = match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => file,
                Err(error) => {
                    self.toast(&format!("Could not open capsule for removal: {error}"));
                    return;
                }
            };
            if locked_file.try_lock_exclusive().is_err() {
                self.toast("Capsule is running; exit it before removal");
                return;
            }
            let trash_file = gio::File::for_path(path);
            if let Err(error) = trash_file.trash(None::<&gio::Cancellable>) {
                self.toast(&format!("Could not move capsule to Trash: {error}"));
                return;
            }
            Some(locked_file)
        } else {
            None
        };
        match self.store.delete(id) {
            Ok(_) => {
                self.records.borrow_mut().retain(|record| record.id != id);
                if let Err(error) = std::fs::remove_file(self.paths.icon_path(id))
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("Could not remove cached Capsule icon: {error}");
                }
                if let Err(error) = std::fs::remove_file(self.paths.legacy_icon_path(id))
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    eprintln!("Could not remove legacy cached Capsule icon: {error}");
                }
                self.refresh();
                self.toast(if record.storage.is_image() {
                    "Capsule moved to Trash"
                } else {
                    "Development entry unregistered"
                });
            }
            Err(error) => self.toast(&format!("Could not update library: {error}")),
        }
    }

    fn show_doctor(&self) {
        let body = match backend::capabilities::detect_with_environment_override() {
            Ok(capabilities) => format!(
                "{capabilities}\n\nContainer mode shares the Linux kernel. Direct GPU access also shares the graphics driver. Capsule has not received a security audit."
            ),
            Err(error) => format!("Capability configuration is invalid: {error}"),
        };
        let dialog = adw::AlertDialog::new(Some("Runtime status"), Some(&body));
        dialog.add_response("close", "Close");
        dialog.set_close_response("close");
        dialog.present(Some(&self.window));
    }

    fn toast(&self, message: &str) {
        self.overlay.add_toast(adw::Toast::new(message));
    }
}

/// Gamescope currently creates its Wayland toplevel without the standard
/// xdg-activation protocol. Niri therefore treats a Gamescope process started
/// through the detached systemd supervisor as an unrelated background client
/// and leaves its new column unfocused. Keep the workaround narrowly scoped to
/// Niri and to a newly-created Gamescope window from this Start action. KWin
/// and other compositors keep their normal toplevel placement/focus policy and
/// do not require compositor-specific IPC here.
fn niri_gamescope_window_ids() -> Option<HashSet<u64>> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok()?;
    if !desktop
        .split(':')
        .any(|part| part.trim().eq_ignore_ascii_case("niri"))
    {
        return None;
    }

    let output = Command::new("/usr/bin/niri")
        .args(["msg", "--json", "windows"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let windows: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(
        windows
            .as_array()?
            .iter()
            .filter(|window| {
                window.get("app_id").and_then(|value| value.as_str()) == Some("gamescope")
            })
            .filter_map(|window| window.get("id").and_then(|value| value.as_u64()))
            .collect(),
    )
}

/// Keep an AppImage-backed supervisor alive independently from the library UI.
/// AppImage exposes the outer one-file executable through `APPIMAGE`; spawning
/// that file gives every long-running game action its own mounted runtime. A
/// normal source or system installation falls back to the current executable.
fn supervisor_executable() -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("APPIMAGE").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if path.is_absolute() && std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(path);
        }
    }
    std::env::current_exe()
}

fn focus_new_niri_gamescope_window(previous: Option<HashSet<u64>>, expected_title: String) {
    let Some(previous) = previous else {
        return;
    };
    // Wine and Ren'Py can spend tens of seconds preparing their first frame;
    // Gamescope does not necessarily map its host toplevel before then.
    let mut attempts = 0_u16;
    gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
        attempts += 1;
        let Some(current) = niri_gamescope_windows() else {
            return gtk::glib::ControlFlow::Break;
        };
        let mut new_windows = current
            .iter()
            .filter(|id| !previous.contains(id))
            .copied()
            .collect::<Vec<_>>();
        new_windows.sort_unstable();

        let id = if new_windows.len() == 1 {
            new_windows.first().copied()
        } else {
            niri_gamescope_window_with_title(&new_windows, &expected_title)
        };
        if let Some(id) = id {
            let _ = Command::new("/usr/bin/niri")
                .args(["msg", "action", "focus-window", "--id", &id.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return gtk::glib::ControlFlow::Break;
        }

        if attempts >= 240 {
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
}

fn capsule_image_is_locked(record: &CapsuleRecord) -> bool {
    let StorageKind::Image { path } = &record.storage else {
        return false;
    };
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            Fs2FileExt::unlock(&file).ok();
            false
        }
        Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
    }
}

fn niri_gamescope_windows() -> Option<HashSet<u64>> {
    niri_gamescope_window_ids()
}

fn niri_gamescope_window_with_title(candidates: &[u64], expected_title: &str) -> Option<u64> {
    let output = Command::new("/usr/bin/niri")
        .args(["msg", "--json", "windows"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let windows: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    windows.as_array()?.iter().find_map(|window| {
        let id = window.get("id")?.as_u64()?;
        let title = window.get("title")?.as_str()?;
        (candidates.contains(&id) && title == expected_title).then_some(id)
    })
}

fn library_page(
    flow: &gtk::FlowBox,
    add_button: &gtk::Button,
    empty: &gtk::Widget,
) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(Orientation::Vertical, 14);
    content.set_margin_top(28);
    content.set_margin_bottom(36);
    content.set_margin_start(24);
    content.set_margin_end(24);

    let heading = gtk::Box::new(Orientation::Horizontal, 12);
    let section = gtk::Label::builder()
        .label("Library")
        .xalign(0.0)
        .hexpand(true)
        .build();
    section.add_css_class("title-1");
    heading.append(&section);
    heading.append(add_button);
    content.append(&heading);
    content.append(flow);
    empty.set_vexpand(true);
    content.append(empty);

    let clamp = adw::Clamp::builder()
        .maximum_size(1180)
        .tightening_threshold(900)
        .child(&content)
        .build();

    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build()
}

fn empty_page() -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .icon_name("applications-games-symbolic")
        .title("Your library is empty")
        .description("Add a Windows game or app from a folder or archive.")
        .build();
    page.upcast()
}

fn entrypoint_summary(record: &CapsuleRecord) -> String {
    record
        .entrypoint
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Windows application")
        .to_owned()
}

fn access_summary(record: &CapsuleRecord) -> String {
    let network = match record.permissions.network {
        NetworkPolicy::Off => "Offline",
        NetworkPolicy::InternetOnly => "Internet",
        NetworkPolicy::LanOnly => "LAN",
        NetworkPolicy::Lan => "Internet + LAN",
        NetworkPolicy::Custom { .. } => "Limited network",
    };
    let audio = match record.permissions.audio {
        AudioPolicy::Off => "Audio off",
        AudioPolicy::PlaybackOnly => "Playback",
        AudioPolicy::PlaybackAndMicrophone => "Playback + microphone",
    };
    format!("{network}  ·  {audio}")
}

#[allow(clippy::too_many_arguments)]
fn begin_portable_inspection(
    source: backend::portable::PortableSource,
    selection: &Rc<RefCell<Option<PortableSelection>>>,
    automatic_name: &Rc<RefCell<Option<String>>>,
    source_row: &adw::ActionRow,
    name_row: &adw::EntryRow,
    executable_row: &adw::ComboRow,
    executable_names: &gtk::StringList,
    choose_folder: &gtk::Button,
    choose_archive: &gtk::Button,
    create: &gtk::Button,
    password_row: &adw::PasswordEntryRow,
    inspect_password: &gtk::Button,
    pending_archive: &Rc<RefCell<Option<backend::portable::PortableSource>>>,
    archive_password: Option<backend::portable::ArchivePassword>,
) {
    *selection.borrow_mut() = None;
    executable_names.splice(0, executable_names.n_items(), &[]);
    executable_row.set_sensitive(false);
    create.set_sensitive(false);
    choose_folder.set_sensitive(false);
    choose_archive.set_sensitive(false);
    inspect_password.set_sensitive(false);
    if matches!(source, backend::portable::PortableSource::Archive(_)) {
        *pending_archive.borrow_mut() = Some(source.clone());
    } else {
        *pending_archive.borrow_mut() = None;
        password_row.set_text("");
        password_row.set_visible(false);
    }
    if archive_password.is_some() {
        password_row.set_visible(true);
    }
    let source_display = source.path().to_string_lossy().into_owned();
    source_row.set_subtitle(&format!("Inspecting {source_display}…"));

    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = backend::portable::inspect_portable_source_with_password(
            &source,
            &backend::portable::ImportLimits::default(),
            archive_password.as_ref(),
        );
        let _ = sender.send((source, archive_password, result));
    });

    let selection = Rc::clone(selection);
    let automatic_name = Rc::clone(automatic_name);
    let source_row = source_row.clone();
    let name_row = name_row.clone();
    let executable_row = executable_row.clone();
    let executable_names = executable_names.clone();
    let choose_folder = choose_folder.clone();
    let choose_archive = choose_archive.clone();
    let create = create.clone();
    let password_row = password_row.clone();
    let inspect_password = inspect_password.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        match receiver.try_recv() {
            Ok((source, archive_password, Ok(inspection))) => {
                let current_name = name_row.text().to_string();
                let replace_name = current_name.trim().is_empty()
                    || automatic_name.borrow().as_deref() == Some(current_name.as_str());
                if replace_name {
                    name_row.set_text(&inspection.suggested_name);
                }
                *automatic_name.borrow_mut() = Some(inspection.suggested_name.clone());

                let labels: Vec<_> = inspection
                    .executable_candidates
                    .iter()
                    .zip(&inspection.candidate_runners)
                    .map(|(candidate, runner)| {
                        let path = candidate
                            .strip_prefix(backend::portable::PORTABLE_GAME_ROOT)
                            .unwrap_or(candidate)
                            .to_string_lossy()
                            .into_owned();
                        let platform = match runner {
                            RunnerKind::Wine => "Windows",
                            RunnerKind::Native => "Linux",
                        };
                        format!("{path} — {platform}")
                    })
                    .collect();
                let label_refs: Vec<_> = labels.iter().map(String::as_str).collect();
                executable_names.splice(0, executable_names.n_items(), &label_refs);
                executable_row.set_selected(inspection.recommended_candidate as u32);
                executable_row.set_sensitive(true);
                let image_size_mib =
                    backend::portable::recommended_image_size_mib(inspection.uncompressed_bytes);
                source_row.set_subtitle(&format!(
                    "{} — {} items, {:.1} MiB\nCapsule capacity: {:.2} GiB",
                    source.path().display(),
                    inspection.entries,
                    inspection.uncompressed_bytes as f64 / (1024.0 * 1024.0),
                    image_size_mib as f64 / 1024.0,
                ));
                password_row.set_visible(archive_password.is_some());
                inspect_password.set_sensitive(archive_password.is_some());
                *selection.borrow_mut() = Some(PortableSelection {
                    source,
                    inspection,
                    archive_password,
                });
                create.set_sensitive(true);
                choose_folder.set_sensitive(true);
                choose_archive.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
            Ok((
                _,
                archive_password,
                Err(backend::portable::PortableImportError::EncryptedArchive),
            )) => {
                let message = if archive_password.is_some() {
                    "Password was not accepted. Check it and try again."
                } else {
                    "This archive is password-protected. Enter its password to continue."
                };
                source_row.set_subtitle(message);
                password_row.set_visible(true);
                password_row.grab_focus();
                inspect_password.set_sensitive(true);
                choose_folder.set_sensitive(true);
                choose_archive.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
            Ok((_, _, Err(backend::portable::PortableImportError::DownloadIncomplete(_)))) => {
                source_row.set_subtitle(
                    "Download is still in progress. Wait for the browser to finish, then choose the archive again.",
                );
                inspect_password.set_sensitive(password_row.is_visible());
                choose_folder.set_sensitive(true);
                choose_archive.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
            Ok((_, _, Err(error))) => {
                source_row.set_subtitle(&format!("Source rejected: {error}"));
                inspect_password.set_sensitive(password_row.is_visible());
                choose_folder.set_sensitive(true);
                choose_archive.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                source_row.set_subtitle("Source inspection stopped unexpectedly");
                inspect_password.set_sensitive(password_row.is_visible());
                choose_folder.set_sensitive(true);
                choose_archive.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
        }
    });
}

fn profile_description(selected: u32) -> &'static str {
    match selected {
        0 => "No GPU or network. Software display support is not implemented yet.",
        1 => "Direct GPU enabled. Network stays disabled.",
        2 => "Direct GPU and internet requested. Filtered networking is not implemented yet.",
        _ => "Permissions are configured individually below.",
    }
}

fn permissions_for_selection(selected: u32, playback: bool, microphone: bool) -> Permissions {
    let mut permissions = match selected {
        1 => Permissions::offline_game(),
        2 => Permissions::online_game(),
        _ => Permissions::locked(),
    };
    permissions.audio = audio_policy_from_rows(playback, microphone);
    if !matches!(permissions.audio, AudioPolicy::Off) {
        permissions.isolation_profile = infer_profile(&permissions);
    }
    permissions
}

fn bind_audio_rows(playback: &adw::SwitchRow, microphone: &adw::SwitchRow) {
    let playback_for_microphone = playback.clone();
    microphone.connect_active_notify(move |microphone| {
        if microphone.is_active() {
            playback_for_microphone.set_active(true);
        }
    });
    let microphone_for_playback = microphone.clone();
    playback.connect_active_notify(move |playback| {
        if !playback.is_active() {
            microphone_for_playback.set_active(false);
        }
    });
}

fn audio_policy_from_rows(playback: bool, microphone: bool) -> AudioPolicy {
    if microphone {
        AudioPolicy::PlaybackAndMicrophone
    } else if playback {
        AudioPolicy::PlaybackOnly
    } else {
        AudioPolicy::Off
    }
}

fn infer_profile(permissions: &Permissions) -> IsolationProfile {
    let mut normalized = permissions.clone();
    normalized.isolation_profile = IsolationProfile::Locked;
    if normalized == Permissions::locked() {
        IsolationProfile::Locked
    } else {
        normalized.isolation_profile = IsolationProfile::OfflineGame;
        if normalized == Permissions::offline_game() {
            IsolationProfile::OfflineGame
        } else {
            normalized.isolation_profile = IsolationProfile::OnlineGame;
            if normalized == Permissions::online_game() {
                IsolationProfile::OnlineGame
            } else {
                IsolationProfile::Custom
            }
        }
    }
}

fn nonzero_u64(value: f64) -> Option<u64> {
    let value = value.round().max(0.0) as u64;
    (value > 0).then_some(value)
}

fn nonzero_u32(value: f64) -> Option<u32> {
    let value = value.round().clamp(0.0, u32::MAX as f64) as u32;
    (value > 0).then_some(value)
}

fn wine_desktop_size_for_settings(configured: Option<WineVirtualDesktop>) -> WineVirtualDesktop {
    configured.unwrap_or(DEFAULT_WINE_VIRTUAL_DESKTOP)
}

fn show_startup_error(application: &adw::Application, message: &str) {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Capsule could not start")
        .description(message)
        .build();
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title(APP_NAME)
        .default_width(620)
        .default_height(420)
        .content(&page)
        .build();
    window.present();
}

#[allow(dead_code)]
fn path_exists_inside(root: &Path, relative: &Path) -> bool {
    is_safe_relative(relative) && root.join(relative).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_capsule_audio_permissions_are_independent() {
        let silent = permissions_for_selection(1, false, false);
        assert_eq!(silent.isolation_profile, IsolationProfile::OfflineGame);
        assert_eq!(silent.audio, AudioPolicy::Off);

        let playback = permissions_for_selection(1, true, false);
        assert_eq!(playback.isolation_profile, IsolationProfile::Custom);
        assert_eq!(playback.audio, AudioPolicy::PlaybackOnly);
        assert_eq!(playback.network, NetworkPolicy::Off);
        assert!(playback.gpu);

        let microphone = permissions_for_selection(1, true, true);
        assert_eq!(microphone.isolation_profile, IsolationProfile::Custom);
        assert_eq!(microphone.audio, AudioPolicy::PlaybackAndMicrophone);
    }

    #[test]
    fn virtual_desktop_switch_starts_at_800x600_and_preserves_saved_sizes() {
        assert_eq!(
            wine_desktop_size_for_settings(None),
            DEFAULT_WINE_VIRTUAL_DESKTOP
        );
        assert_eq!(DEFAULT_WINE_VIRTUAL_DESKTOP.width, 800);
        assert_eq!(DEFAULT_WINE_VIRTUAL_DESKTOP.height, 600);

        let saved = WineVirtualDesktop {
            width: 1024,
            height: 768,
        };
        assert_eq!(wine_desktop_size_for_settings(Some(saved)), saved);
    }

    #[test]
    fn running_image_lock_is_visible_to_the_library_ui() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("game.capsule");
        std::fs::write(&image, b"capsule").unwrap();
        let record = CapsuleRecord::new(
            "Game",
            StorageKind::Image {
                path: image.clone(),
            },
            "drive_c/Game/game.exe",
            RunnerKind::Wine,
        );

        assert!(!capsule_image_is_locked(&record));
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(image)
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        assert!(capsule_image_is_locked(&record));
        Fs2FileExt::unlock(&lock).unwrap();
        assert!(!capsule_image_is_locked(&record));
    }
}
