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
    .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
}

fn should_walk_entry(_path: &Path) -> bool {
  true
}

pub(crate) fn application_search_roots() -> Vec<PathBuf> {
  let mut roots = vec![
    PathBuf::from("/usr/share/applications"),
    PathBuf::from("/usr/local/share/applications"),
  ];

  if let Some(home) = dirs::home_dir() {
    roots.push(home.join(".local").join("share").join("applications"));
  }

  roots
}

pub(crate) fn discover_applications() -> Vec<ApplicationEntry> {
  discover_applications_from_roots(application_search_roots(), 2, is_application_path, should_walk_entry)
}

pub(crate) fn create_application_preview(_path: &Path) -> Result<Option<String>, String> {
  Ok(None)
}

pub(crate) fn configure_main_window(_window: &WebviewWindow) {}

pub(crate) fn open_path(path: &Path) -> Result<(), String> {
  Command::new("xdg-open")
    .arg(path)
    .spawn()
    .map(|_| ())
    .map_err(|error| format!("Failed to open path: {error}"))
}
