mod ui;

use adw::prelude::*;
use capsule::{APP_ID, APP_NAME, backend};
use uuid::Uuid;

fn main() -> adw::glib::ExitCode {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--extract-icon")
    {
        let (Some(input), Some(output)) = (arguments.get(1), arguments.get(2)) else {
            eprintln!("usage: capsule --extract-icon INPUT.exe OUTPUT.png");
            return adw::glib::ExitCode::FAILURE;
        };
        let output = std::path::Path::new(output);
        let raw_icon = output.with_extension("ico");
        let result =
            backend::pe_icon::write_executable_icon(std::path::Path::new(input), &raw_icon)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    let selected_frame = format!("{}[0]", raw_icon.display());
                    let magick = std::env::var_os("CAPSULE_MAGICK")
                        .filter(|path| !path.is_empty())
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::path::PathBuf::from("/usr/bin/magick"));
                    if !magick.is_absolute() {
                        return Err("ImageMagick path must be absolute".into());
                    }
                    let mut command = std::process::Command::new(&magick);
                    command.env_clear();
                    // The icon worker supplies only these relocatable runtime
                    // paths inside its already-cleared Bubblewrap environment.
                    for variable in [
                        "LD_LIBRARY_PATH",
                        "MAGICK_CONFIGURE_PATH",
                        "MAGICK_CODER_MODULE_PATH",
                    ] {
                        if let Some(value) = std::env::var_os(variable) {
                            command.env(variable, value);
                        }
                    }
                    let status = command
                        .arg(selected_frame)
                        .args(["-background", "none", "-thumbnail", "128x128"])
                        .arg(output)
                        .status()
                        .map_err(|error| {
                            format!(
                                "could not start PNG converter {}: {error}",
                                magick.display()
                            )
                        })?;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(format!("PNG converter exited unsuccessfully: {status}"))
                    }
                });
        let _ = std::fs::remove_file(raw_icon);
        return match result {
            Ok(()) => adw::glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Capsule icon extraction failed: {error}");
                adw::glib::ExitCode::FAILURE
            }
        };
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--doctor")
    {
        println!("{APP_NAME} runtime doctor");
        match backend::capabilities::detect_with_environment_override() {
            Ok(capabilities) => {
                println!("{capabilities}");
                if capabilities
                    .missing(&backend::capabilities::Capability::ALL)
                    .next()
                    .is_some()
                {
                    return adw::glib::ExitCode::FAILURE;
                }
            }
            Err(error) => {
                eprintln!("Capability configuration is invalid: {error}");
                return adw::glib::ExitCode::FAILURE;
            }
        }
        return adw::glib::ExitCode::SUCCESS;
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--install-audio-integration")
    {
        return match backend::audio::install_user_policy() {
            Ok(paths) => {
                println!(
                    "Installed Capsule audio integration ({} policy files) and restarted the user audio services",
                    paths.len()
                );
                adw::glib::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Capsule audio integration failed: {error}");
                adw::glib::ExitCode::FAILURE
            }
        };
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--supervise")
    {
        let Some(id) = arguments
            .get(1)
            .and_then(|value| value.to_str())
            .and_then(|value| Uuid::parse_str(value).ok())
        else {
            eprintln!("usage: capsule --supervise UUID");
            return adw::glib::ExitCode::FAILURE;
        };
        return match backend::supervisor::run_from_library(id) {
            Ok(_) => adw::glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Capsule launch failed: {error}");
                adw::glib::ExitCode::FAILURE
            }
        };
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--install-steam")
    {
        let Some(id) = arguments
            .get(1)
            .and_then(|value| value.to_str())
            .and_then(|value| Uuid::parse_str(value).ok())
        else {
            eprintln!("usage: capsule --install-steam UUID");
            return adw::glib::ExitCode::FAILURE;
        };
        let result = (|| {
            let paths = capsule::paths::AppPaths::discover().map_err(|error| error.to_string())?;
            let capabilities = backend::capabilities::detect_with_environment_override()
                .map_err(|error| error.to_string())?;
            eprintln!("Capsule: downloading the official Steam installer from Valve");
            let installer = backend::steam::download_installer(&paths.cache_dir, &capabilities)
                .map_err(|error| error.to_string())?;
            backend::supervisor::install_steam_from_library(id, &installer)
                .map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        })();
        return match result {
            Ok(()) => adw::glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Capsule Steam installation failed: {error}");
                adw::glib::ExitCode::FAILURE
            }
        };
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "--open-steam")
    {
        let Some(id) = arguments
            .get(1)
            .and_then(|value| value.to_str())
            .and_then(|value| Uuid::parse_str(value).ok())
        else {
            eprintln!("usage: capsule --open-steam UUID");
            return adw::glib::ExitCode::FAILURE;
        };
        return match backend::supervisor::open_steam_from_library(id) {
            Ok(_) => adw::glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Capsule Steam launch failed: {error}");
                adw::glib::ExitCode::FAILURE
            }
        };
    }

    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_startup(|_| ui::install_style());
    application.connect_activate(ui::build);
    application.run()
}
