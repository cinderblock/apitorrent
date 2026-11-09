//! The apitorrent desktop app (Tauri 2).
//!
//! The window shows the same web UI the daemon serves, built once from `web/`. Every API
//! call and the sync socket go through Rust (see `commands`), to a remote daemon over
//! strictly verified TLS with a token from the OS keychain, or to the core running in this
//! process. The webview itself never talks to the network.

mod backend;
mod commands;
mod finish;
mod links;
mod local;
mod mapping;
mod notify;
mod profiles;
mod state;

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager as _, Runtime, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::state::{AppState, Dirs};

const MAIN_WINDOW: &str = "main";

/// Show, unminimise and focus the main window.
pub(crate) fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Pages the window may show: the app's own bundled UI (and Vite, in development builds).
/// Anything else opens in the default browser instead.
fn is_app_url(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    match url.scheme() {
        "tauri" => host == "localhost",
        "http" | "https" if host == "tauri.localhost" => true,
        "http" if cfg!(debug_assertions) => host == "localhost" && url.port() == Some(5173),
        _ => false,
    }
}

fn dirs() -> Dirs {
    let project = directories::ProjectDirs::from("", "", "apitorrent")
        .expect("the OS reports a home directory");
    let desktop = project.data_dir().join("desktop");
    let downloads = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| desktop.join("downloads"))
        .join("apitorrent");
    Dirs {
        config: project.config_dir().join("desktop"),
        data: desktop,
        downloads,
    }
}

fn tray<R: Runtime>(app: &tauri::App<R>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show apitorrent", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &PredefinedMenuItem::separator(app)?, &quit])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("apitorrent")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let app = tauri::Builder::default()
        // First, so a second launch hands its arguments over and exits before anything
        // else starts (a second core would fight over the database and ports).
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            // Magnet links in a second launch's arguments reach the deep-link handler below
            // (the plugin's `deep-link` feature forwards them); only files are taken here.
            let links = links::from_args(argv.iter().skip(1), std::path::Path::new(&cwd));
            state::deliver_links(app, links::without_magnets(links));
            show_main_window(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::new(dirs(), Arc::new(profiles::Keychain)))
        .invoke_handler(tauri::generate_handler![
            commands::desktop_state,
            commands::profile_save,
            commands::profile_delete,
            commands::profile_activate,
            commands::profile_disconnect,
            commands::trust_probe,
            commands::trust_pair,
            commands::api_request,
            commands::sync_open,
            commands::sync_close,
            commands::take_pending_links,
            commands::mappings_check,
            commands::mapping_suggestions,
            commands::path_action,
            commands::path_available,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
                .title("apitorrent")
                .inner_size(1280.0, 800.0)
                .min_inner_size(720.0, 480.0)
                .on_navigation(move |url| {
                    if is_app_url(url) {
                        return true;
                    }
                    if matches!(url.scheme(), "http" | "https" | "mailto") {
                        let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
                    }
                    false
                })
                .build()?;
            tray(app)?;

            // magnet: links while running: from macOS open events, or from a second launch
            // on Windows and Linux, forwarded by the single-instance plugin.
            {
                use tauri_plugin_deep_link::DeepLinkExt as _;
                let h = handle.clone();
                app.deep_link().on_open_url(move |event| {
                    state::deliver_links(&h, links::from_urls(&event.urls()));
                });
                // A development build isn't installed, so nothing registered the scheme.
                #[cfg(all(debug_assertions, any(windows, target_os = "linux")))]
                if let Err(e) = app.deep_link().register_all() {
                    tracing::warn!(error = %e, "couldn't register the magnet: handler");
                }
            }

            // Links on this launch's command line.
            let cwd = std::env::current_dir().unwrap_or_default();
            state::deliver_links(&handle, links::from_args(std::env::args().skip(1), &cwd));

            // Reconnect with the last profile.
            let state = app.state::<AppState>();
            let active = state.store.lock().expect("not poisoned").active().cloned();
            if let Some(profile) = active {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let state = h.state::<AppState>();
                    if let Err(e) = state::connect(&h, &state, profile).await {
                        tracing::warn!(error = %e, "couldn't reconnect with the last profile");
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps apitorrent running in the tray: downloads go on, and
            // notifications still arrive. Quit from the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == MAIN_WINDOW
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("the app failed to start");

    app.run(|app, event| match event {
        tauri::RunEvent::ExitRequested { .. } => {
            // Save a local core's state before the process ends.
            let state = app.state::<AppState>();
            tauri::async_runtime::block_on(state::disconnect(&state));
        }
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Opened { urls } => {
            // `.torrent` files; the deep-link plugin turns magnet URLs into its own event.
            state::deliver_links(app, links::without_magnets(links::from_urls(&urls)));
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_apps_own_pages_load_in_the_window() {
        let ok = |s: &str| is_app_url(&url::Url::parse(s).unwrap());
        assert!(ok("tauri://localhost/index.html"));
        assert!(ok("http://tauri.localhost/"));
        assert!(ok("https://tauri.localhost/account"));
        assert!(!ok("https://seedbox.example/"));
        assert!(!ok("http://tauri.localhost.evil.example/"));
        assert!(!ok("tauri://evil/"));
        assert!(!ok("file:///C:/x.html"));
        assert_eq!(ok("http://localhost:5173/"), cfg!(debug_assertions));
        assert!(!ok("http://localhost:8742/"));
    }
}
