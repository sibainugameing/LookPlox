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
  application_extension_trimmed, discover_applications_from_roots, ApplicationEntry,
};

#[cfg(target_os = "linux")]
pub(crate) use linux::{
  application_search_roots, configure_main_window, create_application_preview,
  discover_applications, is_application_path, open_path,
};

#[cfg(target_os = "macos")]
pub(crate) use macos::{
  application_search_roots, configure_main_window, create_application_preview,
  discover_applications, is_application_path, open_path,
};

#[cfg(target_os = "windows")]
pub(crate) use windows::{
  application_search_roots, configure_main_window, create_application_preview,
  discover_applications, is_application_path, open_path,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub(crate) use unsupported::{
  application_search_roots, configure_main_window, create_application_preview,
  discover_applications, is_application_path, open_path,
};
