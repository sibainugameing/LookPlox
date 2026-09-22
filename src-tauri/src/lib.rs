use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{
  atomic::{AtomicBool, AtomicUsize, Ordering},
  Arc, Mutex,
};
use std::time::Duration;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{
  Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, Term, TantivyDocument};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use walkdir::WalkDir;

pub struct AppState {
  pub engine: Arc<SearchEngine>,
  pub indexing: Arc<IndexingState>,
  pub initialized: Arc<AtomicBool>,
  pub watcher_started: Arc<AtomicBool>,
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

pub struct SearchEngine {
  index: Index,
  reader: IndexReader,
  writer: Mutex<IndexWriter>,
  name_field: Field,
  path_field: Field,
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

    let tokenizer = TextAnalyzer::builder(NgramTokenizer::new(2, 8, false)?)
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

  pub fn add_file(&self, path: &Path) -> tantivy::Result<()> {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
      return Ok(());
    };

    let path_string = path.to_string_lossy().into_owned();

    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.add_document(doc!(
      self.name_field => name.to_string(),
      self.path_field => path_string,
    ))?;

    Ok(())
  }

  pub fn upsert_path(&self, path: &Path) -> tantivy::Result<()> {
    if !matches!(std::fs::metadata(path), Ok(metadata) if metadata.is_file()) {
      return Ok(());
    }

    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
      return Ok(());
    };

    let path_string = path.to_string_lossy().into_owned();

    let mut writer = self
      .writer
      .lock()
      .map_err(|_| tantivy::TantivyError::SystemError("writer lock poisoned".into()))?;

    writer.delete_term(Term::from_field_text(self.path_field, &path_string));

    writer.add_document(doc!(
      self.name_field => name.to_string(),
      self.path_field => path_string,
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
    if query.trim().is_empty() {
      return Ok(Vec::new());
    }

    let searcher = self.reader.searcher();
    let mut query_parser =
      tantivy::query::QueryParser::for_index(&self.index, vec![self.name_field]);

    query_parser.set_conjunction_by_default();

    let parsed = query_parser
      .parse_query(query)
      .map_err(|error| error.to_string())?;

    let top_docs = searcher
      .search(&parsed, &TopDocs::with_limit(limit.clamp(1, 50)).order_by_score())
      .map_err(|error| error.to_string())?;

    let mut results = Vec::with_capacity(top_docs.len());

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

      if !name.is_empty() && !path.is_empty() {
        results.push(SearchResult { name, path });
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
    for entry in WalkDir::new(root)
      .follow_links(false)
      .into_iter()
      .filter_map(Result::ok)
    {
      if indexing.cancel_requested.load(Ordering::Relaxed) {
        return Err("Indexing canceled.".into());
      }

      if indexing.indexed.load(Ordering::Relaxed) % 5000 == 0
        && indexing.indexed.load(Ordering::Relaxed) != 0
      {
        engine.commit().map_err(|error| error.to_string())?;
      }

      let entry = entry;
      if !entry.file_type().is_file() {
        continue;
      }

      let path = entry.path();
      indexing.indexed.fetch_add(1, Ordering::Relaxed);

      if let Err(error) = engine.add_file(path) {
        eprintln!("LookPlox skipped {:?}: {error}", path);
      }
    }
  }

  if indexing.cancel_requested.load(Ordering::Relaxed) {
    return Err("Indexing canceled.".into());
  }

  engine.commit().map_err(|error| error.to_string())?;
  Ok(())
}

fn process_event(engine: &SearchEngine, event: Event) {
  for path in event.paths {
    if path.is_file() {
      if let Err(error) = engine.upsert_path(&path) {
        eprintln!("LookPlox failed to update {:?}: {error}", path);
      }
      if let Err(error) = engine.commit() {
        eprintln!("LookPlox failed to commit {:?}: {error}", path);
      }
    } else if !path.exists() {
      if let Err(error) = engine.remove_path(&path) {
        eprintln!("LookPlox failed to remove {:?}: {error}", path);
      }
    }
  }
}

fn start_watcher(engine: Arc<SearchEngine>, roots: Vec<PathBuf>) {
  std::thread::spawn(move || {
    let (tx, rx) = std::sync::mpsc::channel::<NotifyResult<Event>>();

    let mut watcher = match RecommendedWatcher::new(
      move |result| {
        let _ = tx.send(result);
      },
      Config::default().with_poll_interval(Duration::from_millis(500)),
    ) {
      Ok(watcher) => watcher,
      Err(error) => {
        eprintln!("LookPlox failed to start filesystem watcher: {error}");
        return;
      }
    };

    for root in roots {
      if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
        eprintln!("LookPlox failed to watch {:?}: {error}", root);
      }
    }

    while let Ok(result) = rx.recv() {
      match result {
        Ok(event) => process_event(&engine, event),
        Err(error) => eprintln!("LookPlox filesystem watcher error: {error}"),
      }
    }
  });
}

fn start_watcher_once(app_state: &AppState, roots: Vec<PathBuf>) {
  if app_state
    .watcher_started
    .swap(true, Ordering::SeqCst)
  {
    return;
  }

  start_watcher(Arc::clone(&app_state.engine), roots);
}

#[tauri::command]
fn get_setup_state(app: AppHandle) -> Result<SetupState, String> {
  let marker = marker_path(&app)?;
  let roots = read_roots(&app)?;
  let initialized = marker.exists() && !roots.is_empty();

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

  if state
    .indexing
    .running
    .swap(true, Ordering::SeqCst)
  {
    return Err("Indexing is already running.".into());
  }

  state.initialized.store(false, Ordering::SeqCst);
  state.indexing.cancel_requested.store(false, Ordering::SeqCst);
  state.indexing.indexed.store(0, Ordering::Relaxed);

  if let Ok(mut error) = state.indexing.error.lock() {
    *error = None;
  }

  let marker = marker_path(&app)?;
  save_roots(&app, &normalized)?;
  let _ = std::fs::remove_file(&marker);

  let engine = Arc::clone(&state.engine);
  let indexing = Arc::clone(&state.indexing);
  let initialized = Arc::clone(&state.initialized);
  let watcher_started = Arc::clone(&state.watcher_started);
  let watcher_engine = Arc::clone(&state.engine);
  let watcher_roots = normalized.clone();

  std::thread::spawn(move || {
    let result = initial_scan(engine, normalized, Arc::clone(&indexing));

    match result {
      Ok(()) => {
        if let Err(error) = std::fs::write(&marker, b"1") {
          if let Ok(mut status_error) = indexing.error.lock() {
            *status_error = Some(format!("Could not save initialization state: {error}"));
          }
          initialized.store(false, Ordering::SeqCst);
        } else {
          initialized.store(true, Ordering::SeqCst);

          if !watcher_started.swap(true, Ordering::SeqCst) {
            start_watcher(watcher_engine, watcher_roots);
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
  let watcher_started = Arc::new(AtomicBool::new(false));

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
      let watcher_started = Arc::clone(&watcher_started);
      move |app| {
        let data_dir = app.path().app_data_dir()?;
        std::fs::create_dir_all(&data_dir)?;

        let index_dir = data_dir.join("index");
        let engine = Arc::new(SearchEngine::open(&index_dir)?);

        let roots = read_roots(app.handle())?;
        let is_initialized = marker_path(app.handle())?.exists() && !roots.is_empty();

        initialized.store(is_initialized, Ordering::SeqCst);

        app.manage(AppState {
          engine: Arc::clone(&engine),
          indexing: Arc::clone(&indexing),
          initialized: Arc::clone(&initialized),
          watcher_started: Arc::clone(&watcher_started),
        });

        if is_initialized {
          start_watcher(Arc::clone(&engine), roots);
          watcher_started.store(true, Ordering::SeqCst);
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
      search_files,
      open_path
    ])
    .run(tauri::generate_context!())
    .expect("error while running LookPlox");
}
