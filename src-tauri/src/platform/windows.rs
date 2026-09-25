use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::WebviewWindow;

use super::{
  discover_applications_from_roots, ApplicationEntry,
};

pub(crate) fn is_application_path(path: &Path) -> bool {
  path
    .extension()
    .and_then(|extension| extension.to_str())
    .is_some_and(|extension| {
      extension.eq_ignore_ascii_case("exe") || extension.eq_ignore_ascii_case("lnk")
    })
}

pub(crate) fn is_application_container(_path: &Path) -> bool {
  false
}

pub(crate) fn is_inside_application_container(_path: &Path) -> bool {
  false
}

pub(crate) fn should_walk_entry(_path: &Path) -> bool {
  true
}

pub(crate) fn application_search_roots() -> Vec<PathBuf> {
  let mut roots = Vec::new();

  if let Some(value) = std::env::var_os("ProgramFiles") {
    roots.push(PathBuf::from(value));
  }

  if let Some(value) = std::env::var_os("ProgramFiles(x86)") {
    roots.push(PathBuf::from(value));
  }

  if let Some(value) = std::env::var_os("LOCALAPPDATA") {
    roots.push(PathBuf::from(value));
  }

  if let Some(value) = std::env::var_os("APPDATA") {
    let base = PathBuf::from(value);
    roots.push(base.clone());
    roots.push(
      base
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs"),
    );
  }

  if let Some(value) = std::env::var_os("ProgramData") {
    roots.push(
      PathBuf::from(value)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs"),
    );
  }

  roots
}

pub(crate) fn discover_applications() -> Vec<ApplicationEntry> {
  discover_applications_from_roots(application_search_roots(), 4, is_application_path, should_walk_entry)
}

pub(crate) fn create_application_preview(_path: &Path) -> Result<Option<String>, String> {
  Ok(None)
}

pub(crate) fn configure_main_window(_window: &WebviewWindow) {}

pub(crate) fn open_path(path: &Path) -> Result<(), String> {
  Command::new("explorer.exe")
    .arg(path)
    .spawn()
    .map(|_| ())
    .map_err(|error| format!("Failed to open path: {error}"))
}
