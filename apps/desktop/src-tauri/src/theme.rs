//! The colour theme for the window itself: the title bar, the webview's
//! `prefers-color-scheme`, and the background shown before the page paints and around it
//! while resizing.
//!
//! The web UI owns the choice (System, Light or Dark, in the webview's `localStorage`) and
//! tells the app through [`theme_set`]; the app keeps its own copy in `theme.json` so the
//! window opens in the right colours, before any page has loaded.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::window::Color;
use tauri::{AppHandle, Manager as _, Runtime, State, Theme, WebviewWindow};

use crate::state::AppState;

/// The person's choice. Mirrors `ThemePreference` in `web/src/theme.ts`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    /// The window theme to force, or `None` to follow the OS.
    pub fn window_theme(self) -> Option<Theme> {
        match self {
            Self::System => None,
            Self::Light => Some(Theme::Light),
            Self::Dark => Some(Theme::Dark),
        }
    }
}

/// `--bg` in `web/src/theme.css`, for each theme. A test holds them in step.
const LIGHT_BACKGROUND: Color = Color(0xf6, 0xf7, 0xf9, 0xff);
const DARK_BACKGROUND: Color = Color(0x11, 0x14, 0x18, 0xff);

/// The page background for a resolved theme.
pub fn background(theme: Theme) -> Color {
    match theme {
        Theme::Dark => DARK_BACKGROUND,
        _ => LIGHT_BACKGROUND,
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    theme: ThemePreference,
}

fn file(config: &Path) -> PathBuf {
    config.join("theme.json")
}

/// The saved choice; System when there is none, or it can't be read.
pub fn load(config: &Path) -> ThemePreference {
    std::fs::read(file(config))
        .ok()
        .and_then(|raw| serde_json::from_slice::<Stored>(&raw).ok())
        .unwrap_or_default()
        .theme
}

fn save(config: &Path, theme: ThemePreference) -> std::io::Result<()> {
    std::fs::create_dir_all(config)?;
    let path = file(config);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&Stored { theme })?)?;
    std::fs::rename(tmp, path)
}

/// Put the window in the chosen theme, and paint its background to match.
pub fn apply<R: Runtime>(window: &WebviewWindow<R>, theme: ThemePreference) -> tauri::Result<()> {
    window.set_theme(theme.window_theme())?;
    follow(window, window.theme()?);
    Ok(())
}

/// Paint the background for the theme the window is now in: called after [`apply`] and when
/// the OS theme changes under a window that follows it.
pub fn follow<R: Runtime>(window: &WebviewWindow<R>, theme: Theme) {
    if let Err(e) = window.set_background_color(Some(background(theme))) {
        tracing::warn!(error = %e, "couldn't set the window background");
    }
}

/// The web UI's theme changed: remember it, and bring the window along.
// Tauri hands commands their arguments by value.
#[tauri::command]
#[allow(clippy::needless_pass_by_value)]
pub fn theme_set<R: Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    theme: ThemePreference,
) -> Result<(), String> {
    if load(&state.dirs.config) != theme {
        save(&state.dirs.config, theme).map_err(|e| format!("couldn't save the theme: {e}"))?;
    }
    if let Some(window) = app.get_webview_window(crate::MAIN_WINDOW) {
        apply(&window, theme).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: Color) -> String {
        format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
    }

    #[test]
    fn backgrounds_match_the_web_ui() {
        let css = include_str!("../../../../web/src/theme.css");
        let bg = css
            .lines()
            .find_map(|l| l.trim().strip_prefix("--bg: light-dark("))
            .expect("theme.css defines --bg with light-dark()");
        let (light, dark) = bg
            .trim_end_matches(");")
            .split_once(", ")
            .expect("two colours");
        assert_eq!(hex(LIGHT_BACKGROUND), light);
        assert_eq!(hex(DARK_BACKGROUND), dark);
    }

    #[test]
    fn the_choice_round_trips_and_defaults_to_system() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), ThemePreference::System);
        save(dir.path(), ThemePreference::Dark).unwrap();
        assert_eq!(load(dir.path()), ThemePreference::Dark);
        std::fs::write(file(dir.path()), b"{\"theme\":\"mauve\"}").unwrap();
        assert_eq!(load(dir.path()), ThemePreference::System);
    }

    #[test]
    fn the_wire_names_match_the_web_ui() {
        for (t, name) in [
            (ThemePreference::System, "\"system\""),
            (ThemePreference::Light, "\"light\""),
            (ThemePreference::Dark, "\"dark\""),
        ] {
            assert_eq!(serde_json::to_string(&t).unwrap(), name);
        }
    }
}
