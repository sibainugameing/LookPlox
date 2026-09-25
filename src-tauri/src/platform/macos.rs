use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tauri::WebviewWindow;
use window_vibrancy::{
  apply_liquid_glass, apply_vibrancy, LiquidGlassOptions, NSGlassEffectViewStyle,
  NSVisualEffectMaterial, NSVisualEffectState,
};

use super::{
  bytes_to_data_url, discover_applications_from_roots, ApplicationEntry,
};

pub(crate) fn is_application_path(path: &Path) -> bool {
  path
    .extension()
    .and_then(|extension| extension.to_str())
    .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

pub(crate) fn is_application_container(path: &Path) -> bool {
  is_application_path(path)
}

pub(crate) fn is_inside_application_container(path: &Path) -> bool {
  path.ancestors().skip(1).any(is_application_container)
}

pub(crate) fn should_walk_entry(path: &Path) -> bool {
  !is_inside_application_container(path)
}

pub(crate) fn application_search_roots() -> Vec<PathBuf> {
  let mut roots = vec![
    PathBuf::from("/Applications"),
    PathBuf::from("/System/Applications"),
    PathBuf::from("/System/Library/CoreServices"),
  ];

  if let Some(home) = dirs::home_dir() {
    roots.push(home.join("Applications"));
  }

  roots
}

fn run_command_with_timeout(
  program: &str,
  args: &[&OsStr],
  timeout: Duration,
) -> Option<Output> {
  let mut child = Command::new(program)
    .args(args)
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()
    .ok()?;

  let deadline = Instant::now() + timeout;

  loop {
    match child.try_wait() {
      Ok(Some(_)) => return child.wait_with_output().ok(),
      Ok(None) if Instant::now() >= deadline => {
        let _ = child.kill();
        let _ = child.wait();
        return None;
      }
      Ok(None) => std::thread::sleep(Duration::from_millis(40)),
      Err(_) => {
        let _ = child.kill();
        let _ = child.wait();
        return None;
      }
    }
  }
}

fn discover_with_spotlight() -> Vec<ApplicationEntry> {
  let args = [
    OsStr::new("kMDItemContentType == 'com.apple.application-bundle'"),
  ];

  let Some(output) = run_command_with_timeout("/usr/bin/mdfind", &args, Duration::from_secs(2))
  else {
    return Vec::new();
  };

  if !output.status.success() {
    return Vec::new();
  }

  let Ok(stdout) = String::from_utf8(output.stdout) else {
    return Vec::new();
  };

  stdout
    .lines()
    .filter_map(|line| {
      let path = PathBuf::from(line.trim());
      if !path.is_dir() || !is_application_path(&path) {
        return None;
      }

      let name = path.file_name()?.to_str()?.to_owned();
      Some(ApplicationEntry { name, path })
    })
    .collect()
}

pub(crate) fn discover_applications() -> Vec<ApplicationEntry> {
  let mut entries = discover_with_spotlight();

  let fallback = discover_applications_from_roots(application_search_roots(), 4, is_application_path, should_walk_entry);

  if entries.is_empty() {
    entries = fallback;
  } else {
    let mut seen = entries
      .iter()
      .map(|entry| entry.path.clone())
      .collect();

    for entry in fallback {
      if seen.insert(entry.path.clone()) {
        entries.push(entry);
      }
    }
  }

  entries
}

fn read_plist_icon_name(info_plist: &Path) -> Option<String> {
  let keys = [
    "CFBundleIconFile",
    "CFBundleIconName",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.0",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.1",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.2",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.3",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.4",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.5",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.6",
    "CFBundleIcons.CFBundlePrimaryIcon.CFBundleIconFiles.7",
  ];

  for key in keys {
    let args = [
      OsStr::new("-extract"),
      OsStr::new(key),
      OsStr::new("raw"),
      OsStr::new("-o"),
      OsStr::new("-"),
      info_plist.as_os_str(),
    ];

    let Some(output) = run_command_with_timeout("/usr/bin/plutil", &args, Duration::from_secs(1))
    else {
      continue;
    };

    if !output.status.success() {
      continue;
    }

    let Ok(value) = String::from_utf8(output.stdout) else {
      continue;
    };

    let value = value.trim();
    if !value.is_empty() {
      return Some(value.to_owned());
    }
  }

  None
}

fn find_app_icns(path: &Path) -> Option<PathBuf> {
  let resources = path.join("Contents").join("Resources");
  if !resources.is_dir() {
    return None;
  }

  let info_plist = path.join("Contents").join("Info.plist");
  let icon_name = read_plist_icon_name(&info_plist);

  let mut candidate_names = Vec::new();

  if let Some(name) = icon_name {
    let base_name = Path::new(&name)
      .file_name()
      .and_then(|value| value.to_str())
      .unwrap_or(name.as_str());

    candidate_names.push(base_name.to_owned());

    if Path::new(base_name)
      .extension()
      .and_then(|value| value.to_str())
      .is_none()
    {
      candidate_names.push(format!("{base_name}.icns"));
    }
  }

  for fallback in ["AppIcon.icns", "ApplicationIcon.icns", "app.icns"] {
    candidate_names.push(fallback.to_owned());
  }

  for candidate in &candidate_names {
    let direct = resources.join(candidate);
    if direct.is_file()
      && direct
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("icns"))
    {
      return Some(direct);
    }
  }

  let mut fallback_icns = None;

  for entry in walkdir::WalkDir::new(&resources)
    .follow_links(false)
    .into_iter()
    .filter_map(Result::ok)
  {
    let candidate = entry.path();

    if !candidate.is_file()
      || !candidate
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("icns"))
    {
      continue;
    }

    let Some(file_name) = candidate.file_name().and_then(|value| value.to_str()) else {
      continue;
    };

    if candidate_names
      .iter()
      .any(|name| name.eq_ignore_ascii_case(file_name))
    {
      return Some(candidate.to_path_buf());
    }

    if fallback_icns.is_none() {
      fallback_icns = Some(candidate.to_path_buf());
    }
  }

  fallback_icns
}

fn create_icns_preview(path: &Path, output_dir: &Path) -> Result<Option<String>, String> {
  let Some(icon_path) = find_app_icns(path) else {
    return Ok(None);
  };

  let output_path = output_dir.join("app-icon.png");
  let args = [
    OsStr::new("-s"),
    OsStr::new("format"),
    OsStr::new("png"),
    OsStr::new("-Z"),
    OsStr::new("96"),
    icon_path.as_os_str(),
    OsStr::new("--out"),
    output_path.as_os_str(),
  ];

  let output = run_command_with_timeout("/usr/bin/sips", &args, Duration::from_secs(2));

  if output.as_ref().is_none_or(|value| !value.status.success()) || !output_path.is_file() {
    return Ok(None);
  }

  let bytes = std::fs::read(&output_path).map_err(|error| error.to_string())?;
  Ok(Some(bytes_to_data_url(&bytes, "image/png")))
}

fn create_qlmanage_preview(path: &Path, output_dir: &Path) -> Result<Option<String>, String> {
  let args = [
    OsStr::new("-t"),
    OsStr::new("-s"),
    OsStr::new("96"),
    OsStr::new("-o"),
    output_dir.as_os_str(),
    path.as_os_str(),
  ];

  let Some(output) = run_command_with_timeout("/usr/bin/qlmanage", &args, Duration::from_secs(2))
  else {
    eprintln!("LookPlox qlmanage timed out while previewing {:?}", path);
    return Ok(None);
  };

  if !output.status.success() {
    eprintln!(
      "LookPlox qlmanage could not preview {:?}: exit status {}",
      path, output.status
    );
    return Ok(None);
  }

  let preview_path = std::fs::read_dir(output_dir)
    .map_err(|error| error.to_string())?
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .find(|candidate| {
      candidate
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
    });

  match preview_path {
    Some(preview_path) => {
      let bytes = std::fs::read(preview_path).map_err(|error| error.to_string())?;
      Ok(Some(bytes_to_data_url(&bytes, "image/png")))
    }
    None => Ok(None),
  }
}

pub(crate) fn create_application_preview(path: &Path) -> Result<Option<String>, String> {
  if !is_application_path(path) || !path.is_dir() {
    return Ok(None);
  }

  let timestamp = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_err(|error| error.to_string())?
    .as_nanos();

  let output_dir = std::env::temp_dir().join(format!(
    "lookplox-preview-{}-{timestamp}",
    std::process::id()
  ));

  std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;

  let result = create_icns_preview(path, &output_dir)?
    .or(create_qlmanage_preview(path, &output_dir)?);

  let _ = std::fs::remove_dir_all(&output_dir);
  Ok(result)
}

pub(crate) fn configure_main_window(window: &WebviewWindow) {
  let liquid_glass = LiquidGlassOptions::new(NSGlassEffectViewStyle::Clear)
    .radius(26.0)
    .opaque(false);

  if let Err(error) = apply_liquid_glass(window, liquid_glass) {
    eprintln!("LookPlox liquid glass unavailable: {error}");

    if let Err(error) = apply_vibrancy(
      window,
      NSVisualEffectMaterial::HudWindow,
      Some(NSVisualEffectState::Active),
      Some(18.0),
    ) {
      eprintln!("LookPlox vibrancy unavailable: {error}");
    }
  }
}

pub(crate) fn open_path(path: &Path) -> Result<(), String> {
  Command::new("/usr/bin/open")
    .arg(path)
    .spawn()
    .map(|_| ())
    .map_err(|error| format!("Failed to open path: {error}"))
}
