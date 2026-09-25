mod common;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;

pub(crate) use common::{
  application_extension_trimmed, bytes_to_data_url, discover_applications_from_roots,
  ApplicationEntry,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::{
  configure_main_window, create_application_preview, discover_applications,
  is_application_container, is_application_path, is_inside_application_container,
  open_path, should_walk_entry,
};

#[cfg(target_os = "macos")]
pub(crate) use macos::{
  configure_main_window, create_application_preview, discover_applications,
  is_application_container, is_application_path, is_inside_application_container,
  open_path, should_walk_entry,
};

#[cfg(target_os = "windows")]
pub(crate) use windows::{
  configure_main_window, create_application_preview, discover_applications,
  is_application_container, is_application_path, is_inside_application_container,
  open_path, should_walk_entry,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) use unsupported::{
  configure_main_window, create_application_preview, discover_applications,
  is_application_container, is_application_path, is_inside_application_container,
  open_path, should_walk_entry,
};
