mod apps;
mod cache;
mod catalog;
mod commands;
mod error;
mod http;
mod installer;
mod loader;
mod manifest;
mod operations;
mod package;
mod release;
mod snapshot;
mod store;
mod util;
mod version;
mod view;

pub fn run() {
    let state = match commands::AppState::initialize() {
        Ok(state) => state,
        Err(error) => {
            let description = format!(
                "{}\n\nNexusTools did not modify any plugin files.\n\nDetails: {}",
                error.message,
                if error.details.is_empty() {
                    "No additional details.".to_string()
                } else {
                    error.details.join("\n")
                }
            );
            eprintln!("{description}");
            rfd::MessageDialog::new()
                .set_title("NexusTools could not start")
                .set_description(description)
                .set_level(rfd::MessageLevel::Error)
                .show();
            return;
        }
    };

    let builder = tauri::Builder::default()
        .manage(state)
        .setup(commands::setup)
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::refresh_catalog,
            commands::refresh_applications,
            commands::save_app_override,
            commands::remove_app_override,
            commands::pick_directory,
            commands::clear_metadata_cache,
            commands::install_plugin,
            commands::cancel_job,
            commands::uninstall_plugin,
            commands::open_data_folder,
            commands::get_apps,
        ]);
    if let Err(error) = builder.run(tauri::generate_context!()) {
        eprintln!("NexusTools encountered a fatal error: {error}");
    }
}
