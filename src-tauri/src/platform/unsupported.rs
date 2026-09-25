use std::path::{Path, PathBuf};

use tauri::WebviewWindow;

use super::ApplicationEntry;

pub(crate) fn is_application_path(_path: &Path) -> bool {
  false
}

pub(crate) fn application_search_roots() -> Vec<PathBuf> {
  Vec::new()
}

pub(crate) fn discover_applications() -> Vec<ApplicationEntry> {
  Vec::new()
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

pub(crate) fn create_application_preview(_path: &Path) -> Result<Option<String>, String> {
  Ok(None)
}

pub(crate) fn configure_main_window(_window: &WebviewWindow) {}

pub(crate) fn open_path(_path: &Path) -> Result<(), String> {
  Err("Opening paths is not supported on this platform.".into())
}
