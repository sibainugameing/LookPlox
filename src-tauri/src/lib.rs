use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
  atomic::{AtomicBool, AtomicUsize, Ordering},
  Arc, Mutex,
};
use std::time::Duration;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::query::{BooleanQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::{
  Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, Term, TantivyDocument};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use walkdir::WalkDir;

#[cfg(target_os = "macos")]
use window_vibrancy::{
  apply_liquid_glass, apply_vibrancy, LiquidGlassOptions, NSGlassEffectViewStyle,
  NSVisualEffectMaterial, NSVisualEffectState,
};

pub struct AppState {
  pub engine: Arc<SearchEngine>,
  pub indexing: Arc<IndexingState>,
  pub initialized: Arc<AtomicBool>,
  pub active_roots: Arc<Mutex<Vec<PathBuf>>>,
  pub watched_roots: Arc<Mutex<HashSet<PathBuf>>>,
}

pub struct IndexingState {
  pub running: AtomicBool,
  pub cancel_requested: AtomicBool,
  pub indexed: AtomicUsize,
  pub error: Mutex<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
  pub name: String,
  pub path: String,
  pub is_dir: bool,
}

#[derive(Debug, Serialize)]
pub struct SetupState {
  pub initialized: bool,
  pub roots: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct IndexingStatus {
  pub running: bool,
  pub indexed: usize,
  pub error: Option<String>,
}

const INDEX_VERSION: &str = "5";

fn is_app_bundle(path: &Path) -> bool {
  #[cfg(target_os = "macos")]
  {
    return path
      .extension()
      .and_then(|extension| extension.to_str())
      .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
  }

  #[cfg(not(target_os = "macos"))]
  {
    let _ = path;
    false
  }
}

fn is_inside_app_bundle(path: &Path) -> bool {
  path.ancestors().skip(1).any(is_app_bundle)
}

fn should_walk_entry(path: &Path) -> bool {
  !is_inside_app_bundle(path)
}


pub struct SearchEngine {
  index: Index,
  reader: IndexReader,
  writer: Mutex<IndexWriter>,
  name_field: Field,
  path_field: Field,
  is_dir_field: Field,
}

impl SearchEngine {
  pub fn open(index_dir: &Path) -> tantivy::Result<Self> {
    std::fs::create_dir_all(index_dir)?;

    let index = if index_dir.join("meta.json").exists() {
      Index::open_in_dir(index_dir)?
    } else {
      let mut schema_builder = Schema::builder();

      let name_indexing = TextFieldIndexing::default()
        .set_tokenizer("filename_ngram")
        .set_index_option(IndexRecordOption::Basic);

      let name_options = TextOptions::default()
        .set_indexing_options(name_indexing)
        .set_stored();

      schema_builder.add_text_field("name", name_options);
      schema_builder.add_text_field("path", STRING | STORED);
      schema_builder.add_bool_field("is_dir", STORED);

      Index::create_in_dir(index_dir, schema_builder.build())?
    };

    let name_field = index
      .schema()
      .get_field("name")
      .map_err(|_| tantivy::TantivyError::InvalidArgument("Missing name field".into()))?;

    let path_field = index
      .schema()
      .get_field("path")
      .map_err(|_| tantivy::TantivyError::InvalidArgument("Missing path field".into()))?;

    let is_dir_field = index
      .schema()
      .get_field("is_dir")
      .map_err(|_| tantivy::TantivyError::InvalidArgument("Missing is_dir field".into()))?;

    let tokenizer = TextAnalyzer::builder(NgramTokenizer::new(1, 8, false)?)
      .filter(LowerCaser)
      .build();

    index.tokenizers().register("filename_ngram", tokenizer);

    let reader = index.reader()?;
    let writer = index.writer(64_000_000)?;

    Ok(Self {
      index,
      reader,
      writer: Mutex::new(writer),
      name_field,
      path_field,
      is_dir_field,
    })
  }

  pub fn clear(&self) -> tantivy::Result<()> {
    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.delete_all_documents()?;
    writer.commit()?;
    self.reader.reload()?;
    Ok(())
  }

  pub fn upsert_path(&self, path: &Path) -> tantivy::Result<()> {
    let metadata = match std::fs::metadata(path) {
      Ok(metadata) => metadata,
      Err(_) => return Ok(()),
    };

    if !metadata.is_file() && !metadata.is_dir() {
      return Ok(());
    }

    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
      return Ok(());
    };

    let path_string = path.to_string_lossy().into_owned();
    // macOS .app bundles are directories on disk, but should appear as applications in search results.
    let is_dir = metadata.is_dir() && !is_app_bundle(path);

    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.delete_term(Term::from_field_text(self.path_field, &path_string));

    writer.add_document(doc!(
      self.name_field => name.to_string(),
      self.path_field => path_string,
      self.is_dir_field => is_dir,
    ))?;

    Ok(())
  }

  pub fn remove_path(&self, path: &Path) -> tantivy::Result<()> {
    let path_string = path.to_string_lossy();

    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.delete_term(Term::from_field_text(self.path_field, path_string.as_ref()));
    writer.commit()?;
    self.reader.reload()?;
    Ok(())
  }

  pub fn remove_subtree(&self, path: &Path) -> tantivy::Result<()> {
    let raw_path = path.to_string_lossy().into_owned();
    let normalized_path = raw_path.trim_end_matches(['/', '\\']).to_string();
    let prefix = if normalized_path.is_empty() {
      raw_path.clone()
    } else {
      format!("{}{}", normalized_path, std::path::MAIN_SEPARATOR)
    };

    let escaped_prefix: String = prefix
      .chars()
      .flat_map(|character| {
        if r#"\\.^$|()[]{}*+?"#.contains(character) {
          ['\\', character].into_iter().collect::<Vec<_>>()
        } else {
          [character].into_iter().collect::<Vec<_>>()
        }
      })
      .collect();

    let pattern = format!("^{}", escaped_prefix);
    let searcher = self.reader.searcher();
    let query = RegexQuery::from_pattern(&pattern, self.path_field)?;
    let top_docs = searcher.search(&query, &TopDocs::with_limit(1_000_000).order_by_score())?;

    let mut paths_to_remove = Vec::with_capacity(top_docs.len() + 2);
    if !raw_path.is_empty() {
      paths_to_remove.push(raw_path);
    }
    if !normalized_path.is_empty() {
      paths_to_remove.push(normalized_path);
    }

    for (_, address) in top_docs {
      let document: TantivyDocument = searcher.doc::<TantivyDocument>(address)?;
      if let Some(value) = document
        .get_first(self.path_field)
        .and_then(|value| value.as_str())
      {
        paths_to_remove.push(value.to_owned());
      }
    }

    drop(searcher);

    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    for path_string in paths_to_remove {
      writer.delete_term(Term::from_field_text(self.path_field, &path_string));
    }

    writer.commit()?;
    self.reader.reload()?;
    Ok(())
  }

  pub fn commit(&self) -> tantivy::Result<()> {
    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.commit()?;
    self.reader.reload()?;
    Ok(())
  }

  pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
      return Ok(Vec::new());
    }

    let mut analyzer = self
      .index
      .tokenizers()
      .get("filename_ngram")
      .ok_or_else(|| "Search tokenizer is unavailable.".to_string())?;

    let mut stream = analyzer.token_stream(&normalized);
    let mut tokens = Vec::<String>::new();

    stream.process(&mut |token| {
      if !token.text.is_empty() && !tokens.iter().any(|item| item == &token.text) {
        tokens.push(token.text.clone());
      }
    });

    if tokens.is_empty() {
      return Ok(Vec::new());
    }

    let clauses: Vec<Box<dyn Query>> = tokens
      .iter()
      .map(|token| {
        Box::new(TermQuery::new(
          Term::from_field_text(self.name_field, token),
          IndexRecordOption::Basic,
        )) as Box<dyn Query>
      })
      .collect();

    let parsed: Box<dyn Query> = if clauses.len() == 1 {
      clauses.into_iter().next().expect("search term exists")
    } else {
      Box::new(BooleanQuery::intersection(clauses))
    };

    let searcher = self.reader.searcher();
    let requested_limit = limit.clamp(1, 50);
    let candidate_limit = (requested_limit * 50).clamp(100, 1000);

    let top_docs = searcher
      .search(&parsed, &TopDocs::with_limit(candidate_limit).order_by_score())
      .map_err(|error| error.to_string())?;

    let mut results = Vec::with_capacity(requested_limit);

    for (_, address) in top_docs {
      let doc: TantivyDocument = searcher
        .doc::<TantivyDocument>(address)
        .map_err(|error| error.to_string())?;

      let name = doc
        .get_first(self.name_field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();

      let path = doc
        .get_first(self.path_field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();

      let is_dir = doc
        .get_first(self.is_dir_field)
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

      if !name.is_empty()
        && !path.is_empty()
        && name.to_lowercase().contains(&normalized)
      {
        results.push(SearchResult { name, path, is_dir });

        if results.len() >= requested_limit {
          break;
        }
      }
    }

    Ok(results)
  }

}

fn marker_path(app: &AppHandle) -> Result<PathBuf, String> {
  app.path()
    .app_data_dir()
    .map(|dir| dir.join(".initialized"))
    .map_err(|error| error.to_string())
}

fn roots_path(app: &AppHandle) -> Result<PathBuf, String> {
  app.path()
    .app_data_dir()
    .map(|dir| dir.join("index-roots.json"))
    .map_err(|error| error.to_string())
}

fn index_version_path(app: &AppHandle) -> Result<PathBuf, String> {
  app.path()
    .app_data_dir()
    .map(|dir| dir.join("index-version"))
    .map_err(|error| error.to_string())
}

fn is_index_current(app: &AppHandle) -> Result<bool, String> {
  let path = index_version_path(app)?;
  if !path.exists() {
    return Ok(false);
  }

  let version = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
  Ok(version.trim() == INDEX_VERSION)
}

fn read_roots(app: &AppHandle) -> Result<Vec<PathBuf>, String> {
  let path = roots_path(app)?;
  if !path.exists() {
    return Ok(Vec::new());
  }

  let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
  let roots: Vec<String> =
    serde_json::from_str(&content).map_err(|error| error.to_string())?;

  Ok(roots.into_iter().map(PathBuf::from).collect())
}

fn save_roots(app: &AppHandle, roots: &[PathBuf]) -> Result<(), String> {
  let path = roots_path(app)?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  }

  let serialized: Vec<String> = roots
    .iter()
    .map(|root| root.to_string_lossy().into_owned())
    .collect();

  let content = serde_json::to_string_pretty(&serialized).map_err(|error| error.to_string())?;
  std::fs::write(path, content).map_err(|error| error.to_string())
}

fn initial_scan(
  engine: Arc<SearchEngine>,
  roots: Vec<PathBuf>,
  indexing: Arc<IndexingState>,
) -> Result<(), String> {
  engine.clear().map_err(|error| error.to_string())?;

  for root in roots {
    for entry in WalkDir::new(&root)
      .follow_links(false)
      .into_iter()
      .filter_entry(|entry| should_walk_entry(entry.path()))
      .filter_map(Result::ok)
    {
      if indexing.cancel_requested.load(Ordering::Relaxed) {
        return Err("Indexing canceled.".into());
      }

      if !entry.file_type().is_file() && !entry.file_type().is_dir() {
        continue;
      }

      let path = entry.path();

      if path == root {
        continue;
      }

      indexing.indexed.fetch_add(1, Ordering::Relaxed);

      if let Err(error) = engine.upsert_path(path) {
        eprintln!("LookPlox skipped {:?}: {error}", path);
      }

      if indexing.indexed.load(Ordering::Relaxed) % 5000 == 0 {
        engine.commit().map_err(|error| error.to_string())?;
      }
    }
  }

  if indexing.cancel_requested.load(Ordering::Relaxed) {
    return Err("Indexing canceled.".into());
  }

  engine.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn incremental_scan(
  engine: Arc<SearchEngine>,
  root: PathBuf,
  indexing: Arc<IndexingState>,
) -> Result<(), String> {
  for entry in WalkDir::new(&root)
    .follow_links(false)
    .into_iter()
    .filter_entry(|entry| should_walk_entry(entry.path()))
    .filter_map(Result::ok)
  {
    if indexing.cancel_requested.load(Ordering::Relaxed) {
      return Err("Indexing canceled.".into());
    }

    if !entry.file_type().is_file() && !entry.file_type().is_dir() {
      continue;
    }

    let path = entry.path();

    if path == root {
      continue;
    }

    indexing.indexed.fetch_add(1, Ordering::Relaxed);

    if let Err(error) = engine.upsert_path(path) {
      eprintln!("LookPlox skipped {:?}: {error}", path);
    }

    if indexing.indexed.load(Ordering::Relaxed) % 5000 == 0 {
      engine.commit().map_err(|error| error.to_string())?;
    }
  }

  if indexing.cancel_requested.load(Ordering::Relaxed) {
    return Err("Indexing canceled.".into());
  }

  engine.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn path_is_active(path: &Path, roots: &[PathBuf]) -> bool {
  roots.iter().any(|root| path == root || path.starts_with(root))
}

fn process_event(
  engine: &SearchEngine,
  event: Event,
  active_roots: &Mutex<Vec<PathBuf>>,
) -> bool {
  let roots = match active_roots.lock() {
    Ok(value) => value.clone(),
    Err(_) => return false,
  };

  let mut changed = false;

  for path in event.paths {
    if !path_is_active(&path, &roots) {
      continue;
    }

    if path.is_file() {
      if let Err(error) = engine.upsert_path(&path) {
        eprintln!("LookPlox failed to update {:?}: {error}", path);
      } else {
        changed = true;
      }
    } else if path.is_dir() {
      if is_inside_app_bundle(&path) {
        continue;
      }

      if let Err(error) = engine.upsert_path(&path) {
        eprintln!("LookPlox failed to index folder {:?}: {error}", path);
      } else {
        changed = true;
      }

      if is_app_bundle(&path) {
        continue;
      }

      for entry in WalkDir::new(&path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_walk_entry(entry.path()))
        .filter_map(Result::ok)
      {
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
          continue;
        }

        if entry.path() == path {
          continue;
        }

        if let Err(error) = engine.upsert_path(entry.path()) {
          eprintln!("LookPlox failed to index new file {:?}: {error}", entry.path());
        } else {
          changed = true;
        }
      }
    } else if !path.exists() {
      if let Err(error) = engine.remove_subtree(&path) {
        eprintln!("LookPlox failed to remove {:?}: {error}", path);
      } else {
        changed = true;
      }
    }
  }

  changed
}

fn start_watcher(
  engine: Arc<SearchEngine>,
  root: PathBuf,
  active_roots: Arc<Mutex<Vec<PathBuf>>>,
) {
  std::thread::spawn(move || {
    let (tx, rx) = std::sync::mpsc::channel::<NotifyResult<Event>>();

    let mut watcher = match RecommendedWatcher::new(
      move |result| {
        let _ = tx.send(result);
      },
      Config::default().with_poll_interval(Duration::from_millis(300)),
    ) {
      Ok(watcher) => watcher,
      Err(error) => {
        eprintln!("LookPlox failed to start filesystem watcher: {error}");
        return;
      }
    };

    if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
      eprintln!("LookPlox failed to watch {:?}: {error}", root);
      return;
    }

    while let Ok(result) = rx.recv() {
      match result {
        Ok(first_event) => {
          let mut events = vec![first_event];

          while let Ok(Ok(event)) = rx.recv_timeout(Duration::from_millis(150)) {
            events.push(event);
          }

          let mut changed = false;

          for event in events {
            if process_event(&engine, event, &active_roots) {
              changed = true;
            }
          }

          if changed {
            if let Err(error) = engine.commit() {
              eprintln!("LookPlox failed to commit filesystem changes: {error}");
            }
          }
        }
        Err(error) => eprintln!("LookPlox filesystem watcher error: {error}"),
      }
    }
  });
}

fn watch_root_once(
  engine: Arc<SearchEngine>,
  active_roots: Arc<Mutex<Vec<PathBuf>>>,
  watched_roots: Arc<Mutex<HashSet<PathBuf>>>,
  root: PathBuf,
) {
  let mut watched = match watched_roots.lock() {
    Ok(value) => value,
    Err(_) => return,
  };

  if watched.insert(root.clone()) {
    drop(watched);
    start_watcher(engine, root, active_roots);
  }
}

#[tauri::command]
fn get_setup_state(app: AppHandle) -> Result<SetupState, String> {
  let marker = marker_path(&app)?;
  let roots = read_roots(&app)?;
  let initialized = marker.exists() && !roots.is_empty() && is_index_current(&app)?;

  Ok(SetupState {
    initialized,
    roots: roots
      .into_iter()
      .map(|root| root.to_string_lossy().into_owned())
      .collect(),
  })
}

#[tauri::command]
fn get_indexing_status(state: State<'_, AppState>) -> IndexingStatus {
  let error = state
    .indexing
    .error
    .lock()
    .ok()
    .and_then(|value| value.clone());

  IndexingStatus {
    running: state.indexing.running.load(Ordering::Relaxed),
    indexed: state.indexing.indexed.load(Ordering::Relaxed),
    error,
  }
}

#[tauri::command]
fn start_indexing(
  app: AppHandle,
  state: State<'_, AppState>,
  roots: Vec<String>,
) -> Result<(), String> {
  if roots.is_empty() {
    return Err("Add at least one folder to continue.".into());
  }

  let normalized: Vec<PathBuf> = roots
    .into_iter()
    .map(PathBuf::from)
    .filter(|root| root.is_dir())
    .collect();

  if normalized.is_empty() {
    return Err("None of the selected folders are available.".into());
  }

  if state.indexing.running.swap(true, Ordering::SeqCst) {
    return Err("Indexing is already running.".into());
  }

  state.initialized.store(false, Ordering::SeqCst);
  state.indexing.cancel_requested.store(false, Ordering::SeqCst);
  state.indexing.indexed.store(0, Ordering::Relaxed);

  if let Ok(mut error) = state.indexing.error.lock() {
    *error = None;
  }

  let marker = marker_path(&app)?;
  let version_path = index_version_path(&app)?;
  save_roots(&app, &normalized)?;
  let _ = std::fs::remove_file(&marker);
  let _ = std::fs::remove_file(&version_path);

  {
    let mut active = state
      .active_roots
      .lock()
      .map_err(|_| "Index root state is unavailable.".to_string())?;
    *active = normalized.clone();
  }

  if let Ok(mut watched) = state.watched_roots.lock() {
    watched.clear();
  }

  let engine = Arc::clone(&state.engine);
  let indexing = Arc::clone(&state.indexing);
  let initialized = Arc::clone(&state.initialized);
  let active_roots = Arc::clone(&state.active_roots);
  let watched_roots = Arc::clone(&state.watched_roots);

  std::thread::spawn(move || {
    let result = initial_scan(engine.clone(), normalized.clone(), Arc::clone(&indexing));

    match result {
      Ok(()) => {
        if let Err(error) = std::fs::write(&marker, b"1") {
          if let Ok(mut status_error) = indexing.error.lock() {
            *status_error = Some(format!("Could not save initialization state: {error}"));
          }
          initialized.store(false, Ordering::SeqCst);
        } else if let Err(error) = std::fs::write(&version_path, INDEX_VERSION) {
          if let Ok(mut status_error) = indexing.error.lock() {
            *status_error = Some(format!("Could not save index version: {error}"));
          }
          initialized.store(false, Ordering::SeqCst);
        } else {
          initialized.store(true, Ordering::SeqCst);

          for root in normalized {
            watch_root_once(
              Arc::clone(&engine),
              Arc::clone(&active_roots),
              Arc::clone(&watched_roots),
              root,
            );
          }
        }
      }
      Err(error) => {
        initialized.store(false, Ordering::SeqCst);

        if let Ok(mut status_error) = indexing.error.lock() {
          *status_error = Some(error);
        }
      }
    }

    indexing.running.store(false, Ordering::SeqCst);
  });

  Ok(())
}

#[tauri::command]
fn add_index_root(
  app: AppHandle,
  state: State<'_, AppState>,
  root: String,
) -> Result<Vec<String>, String> {
  let root_path = PathBuf::from(root);

  if state.indexing.running.load(Ordering::SeqCst) {
    return Err("Indexing is already running.".into());
  }

  let roots = {
    let mut active = state
      .active_roots
      .lock()
      .map_err(|_| "Index root state is unavailable.".to_string())?;

    if active.iter().any(|item| {
      root_path.starts_with(item) || item.starts_with(&root_path)
    }) {
      return Ok(active
        .iter()
        .map(|item| item.to_string_lossy().into_owned())
        .collect());
    }

    active.push(root_path.clone());
    active.clone()
  };

  save_roots(&app, &roots)?;

  state.indexing.running.store(true, Ordering::SeqCst);
  state.indexing.cancel_requested.store(false, Ordering::SeqCst);
  state.indexing.indexed.store(0, Ordering::Relaxed);

  if let Ok(mut error) = state.indexing.error.lock() {
    *error = None;
  }

  let engine = Arc::clone(&state.engine);
  let indexing = Arc::clone(&state.indexing);
  let active_roots = Arc::clone(&state.active_roots);
  let watched_roots = Arc::clone(&state.watched_roots);
  let root_for_scan = root_path.clone();
  let app_for_worker = app.clone();

  std::thread::spawn(move || {
    let result = incremental_scan(
      Arc::clone(&engine),
      root_for_scan.clone(),
      Arc::clone(&indexing),
    );

    if let Err(error) = result {
      if let Ok(mut active) = active_roots.lock() {
        active.retain(|item| item != &root_for_scan);
      }

      let _ = read_roots(&app_for_worker).and_then(|mut roots| {
        roots.retain(|item| item != &root_for_scan);
        save_roots(&app_for_worker, &roots)
      });

      if let Ok(mut status_error) = indexing.error.lock() {
        *status_error = Some(error);
      }
    } else {
      watch_root_once(engine, active_roots, watched_roots, root_for_scan);
    }

    indexing.running.store(false, Ordering::SeqCst);
  });

  Ok(roots
    .into_iter()
    .map(|item| item.to_string_lossy().into_owned())
    .collect())
}

#[tauri::command]
fn remove_index_root(
  app: AppHandle,
  state: State<'_, AppState>,
  root: String,
) -> Result<Vec<String>, String> {
  let root_path = PathBuf::from(root);

  if !root_path.is_dir() {
    return Err("The selected folder is not available.".into());
  }

  if state.indexing.running.load(Ordering::SeqCst) {
    return Err("Indexing is already running.".into());
  }

  let roots = {
    let mut active = state
      .active_roots
      .lock()
      .map_err(|_| "Index root state is unavailable.".to_string())?;

    if active.len() <= 1 {
      return Err("LookPlox needs at least one tracked folder.".into());
    }

    let before = active.len();
    active.retain(|item| item != &root_path);

    if active.len() == before {
      return Ok(active
        .iter()
        .map(|item| item.to_string_lossy().into_owned())
        .collect());
    }

    active.clone()
  };

  save_roots(&app, &roots)?;
  state.engine.remove_subtree(&root_path).map_err(|error| error.to_string())?;

  if let Ok(mut watched) = state.watched_roots.lock() {
    watched.remove(&root_path);
  }

  Ok(roots
    .into_iter()
    .map(|item| item.to_string_lossy().into_owned())
    .collect())
}

#[tauri::command]
fn cancel_indexing(state: State<'_, AppState>) -> Result<(), String> {
  if !state.indexing.running.load(Ordering::SeqCst) {
    return Ok(());
  }

  state.indexing.cancel_requested.store(true, Ordering::SeqCst);
  Ok(())
}

#[tauri::command]
fn search_files(
  state: State<'_, AppState>,
  query: String,
  limit: Option<usize>,
) -> Result<Vec<SearchResult>, String> {
  state.engine.search(&query, limit.unwrap_or(12))
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
  let path_ref = Path::new(&path);

  if !path_ref.exists() {
    return Err(format!("Path does not exist: {path}"));
  }

  #[cfg(target_os = "macos")]
  std::process::Command::new("open")
    .arg(path_ref)
    .spawn()
    .map_err(|error| format!("Failed to open path: {error}"))?;

  #[cfg(target_os = "windows")]
  std::process::Command::new("cmd")
    .args(["/C", "start", "", &path])
    .spawn()
    .map_err(|error| format!("Failed to open path: {error}"))?;

  #[cfg(target_os = "linux")]
  std::process::Command::new("xdg-open")
    .arg(path_ref)
    .spawn()
    .map_err(|error| format!("Failed to open path: {error}"))?;

  Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  let indexing = Arc::new(IndexingState {
    running: AtomicBool::new(false),
    cancel_requested: AtomicBool::new(false),
    indexed: AtomicUsize::new(0),
    error: Mutex::new(None),
  });

  let initialized = Arc::new(AtomicBool::new(false));


  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .plugin(
      tauri_plugin_global_shortcut::Builder::new()
        .with_handler({
          let initialized = Arc::clone(&initialized);
          move |app, shortcut, event| {
            let hotkey = Shortcut::new(Some(Modifiers::ALT), Code::Space);

            if shortcut == &hotkey && event.state() == ShortcutState::Pressed {
              if let Some(window) = app.get_webview_window("main") {
                if !initialized.load(Ordering::SeqCst) {
                  let _ = window.show();
                  let _ = window.set_focus();
                  return;
                }

                match window.is_visible() {
                  Ok(true) => {
                    let _ = window.hide();
                  }
                  Ok(false) => {
                    let _ = window.show();
                    let _ = window.set_focus();
                  }
                  Err(error) => {
                    eprintln!("LookPlox failed to check window visibility: {error}");
                  }
                }
              }
            }
          }
        })
        .build(),
    )
    .setup({
      let indexing = Arc::clone(&indexing);
      let initialized = Arc::clone(&initialized);
      move |app| {
        let data_dir = app.path().app_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;

        let index_dir = data_dir.join("index");

        if !is_index_current(app.handle())? && index_dir.exists() {
          std::fs::remove_dir_all(&index_dir)?;
        }

        let engine = Arc::new(SearchEngine::open(&index_dir)?);

        let roots = read_roots(app.handle())?;
        let is_initialized =
          marker_path(app.handle())?.exists()
            && !roots.is_empty()
            && is_index_current(app.handle())?;

        #[cfg(target_os = "macos")]
        if let Some(window) = app.get_webview_window("main") {
          let liquid_glass = LiquidGlassOptions::new(NSGlassEffectViewStyle::Clear)
            .radius(26.0)
            .opaque(false);

          if let Err(error) = apply_liquid_glass(&window, liquid_glass) {
            eprintln!("LookPlox liquid glass unavailable: {error}");

            if let Err(error) = apply_vibrancy(
              &window,
              NSVisualEffectMaterial::HudWindow,
              Some(NSVisualEffectState::Active),
              Some(18.0),
            ) {
              eprintln!("LookPlox vibrancy unavailable: {error}");
            }
          }
        }

        initialized.store(is_initialized, Ordering::SeqCst);

        let active_roots = Arc::new(Mutex::new(roots.clone()));
        let watched_roots = Arc::new(Mutex::new(HashSet::new()));

        app.manage(AppState {
          engine: Arc::clone(&engine),
          indexing: Arc::clone(&indexing),
          initialized: Arc::clone(&initialized),
          active_roots: Arc::clone(&active_roots),
          watched_roots: Arc::clone(&watched_roots),
        });

        if is_initialized {
          for root in roots {
            watch_root_once(
              Arc::clone(&engine),
              Arc::clone(&active_roots),
              Arc::clone(&watched_roots),
              root,
            );
          }
        }

        #[cfg(desktop)]
        {
          let hotkey = Shortcut::new(Some(Modifiers::ALT), Code::Space);
          app.global_shortcut().register(hotkey)?;
        }

        Ok(())
      }
    })
    .invoke_handler(tauri::generate_handler![
      get_setup_state,
      get_indexing_status,
      start_indexing,
      cancel_indexing,
      add_index_root,
      remove_index_root,
      search_files,
      open_path
    ])
    .run(tauri::generate_context!())
    .expect("error while running LookPlox");
}
