use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub(crate) struct ApplicationEntry {
  pub(crate) name: String,
  pub(crate) path: PathBuf,
}

pub(crate) fn application_extension_trimmed(name: &str) -> &str {
  for suffix in [".app", ".exe", ".lnk", ".desktop"] {
    if name.len() > suffix.len() && name.to_ascii_lowercase().ends_with(suffix) {
      return &name[..name.len() - suffix.len()];
    }
  }

  name
}

pub(crate) fn discover_applications_from_roots(
  roots: Vec<PathBuf>,
  max_depth: usize,
  is_application_path: fn(&Path) -> bool,
  should_walk_entry: fn(&Path) -> bool,
) -> Vec<ApplicationEntry> {
  let mut entries = Vec::new();
  let mut seen = HashSet::<PathBuf>::new();

  for root in roots {
    if !root.is_dir() {
      continue;
    }

    for entry in WalkDir::new(&root)
      .follow_links(false)
      .max_depth(max_depth)
      .into_iter()
      .filter_entry(|entry| should_walk_entry(entry.path()))
      .filter_map(Result::ok)
    {
      if !entry.file_type().is_file() && !entry.file_type().is_dir() {
        continue;
      }

      let path = entry.path();

      if !is_application_path(path) || !seen.insert(path.to_path_buf()) {
        continue;
      }

      let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        continue;
      };

      entries.push(ApplicationEntry {
        name: name.to_owned(),
        path: path.to_path_buf(),
      });
    }
  }

  entries
}

pub(crate) fn bytes_to_data_url(bytes: &[u8], mime: &str) -> String {
  format!("data:{mime};base64,{}", BASE64_STANDARD.encode(bytes))
}
