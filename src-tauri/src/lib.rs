use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tantivy::collector::TopDocs;
use tantivy::doc;
use tantivy::schema::{\n  Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,\n};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, Term, TantivyDocument};
use tauri::{AppHandle, Manager, State};
use walkdir::WalkDir;

pub struct AppState {
  pub engine: Arc<SearchEngine>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
  pub name: String,
  pub path: String,
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

    let tokenizer = TextAnalyzer::builder(NgramTokenizer::new(2, 16, false)?)
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

  pub fn upsert_path(&self, path: &Path) -> tantivy::Result<()> {
    let metadata = match std::fs::metadata(path) {
      Ok(metadata) if metadata.is_file() => metadata,
      _ => return Ok(()),
    };

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

    // Keep the metadata read here so failed filesystem reads are surfaced
    // during indexing without putting file contents into the search index.
    let _ = metadata.len();

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

    // Every indexed n-gram must match, which turns an n-gram index into a
    // practical substring search for the current MVP.
    query_parser.set_conjunction_by_default();

    let parsed = query_parser
      .parse_query(query)
      .map_err(|error| error.to_string())?;

    let top_docs = searcher
      .search(&parsed, &TopDocs::with_limit(limit.clamp(1, 50)))
      .map_err(|error| error.to_string())?;

    let mut results = Vec::with_capacity(top_docs.len());

    for (_, address) in top_docs {
      let doc: TantivyDocument = searcher
        .doc(address)
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

fn default_roots() -> Vec<PathBuf> {
  let Some(home) = dirs::home_dir() else {
    return Vec::new();
  };

  [
    home.join("Desktop"),
    home.join("Documents"),
    home.join("Downloads"),
    home.join("Pictures"),
    home.join("Music"),
    home.join("Movies"),
  ]
  .into_iter()
  .filter(|path| path.is_dir())
  .collect()
}

fn initial_scan(engine: Arc<SearchEngine>, roots: Vec<PathBuf>) -> Result<(), String> {
  engine.clear().map_err(|error| error.to_string())?;

  let mut indexed_since_commit = 0usize;

  for root in roots {
    for entry in WalkDir::new(root)
      .follow_links(false)
      .into_iter()
      .filter_map(Result::ok)
    {
      let path = entry.path();

      if !path.is_file() {
        continue;
      }

      if let Err(error) = engine.upsert_path(path) {
        eprintln!("LookPlox skipped {:?}: {error}", path);
        continue;
      }

      indexed_since_commit += 1;

      if indexed_since_commit >= 1000 {
        engine.commit().map_err(|error| error.to_string())?;
        indexed_since_commit = 0;
      }
    }
  }

  if indexed_since_commit > 0 {
    engine.commit().map_err(|error| error.to_string())?;
  }

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

fn setup_app(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
  let data_dir = app.path().app_data_dir()?;
  std::fs::create_dir_all(&data_dir)?;

  let index_dir = data_dir.join("index");
  let engine = Arc::new(SearchEngine::open(&index_dir)?);

  app.manage(AppState {
    engine: Arc::clone(&engine),
  });

  let roots = default_roots();

  start_watcher(Arc::clone(&engine), roots.clone());

  if !data_dir.join(".initialized").exists() {
    let engine_for_scan = Arc::clone(&engine);
    let marker = data_dir.join(".initialized");

    std::thread::spawn(move || {
      match initial_scan(engine_for_scan, roots) {
        Ok(()) => {
          if let Err(error) = std::fs::write(marker, b"1") {
            eprintln!("LookPlox failed to write index marker: {error}");
          }
        }
        Err(error) => {
          eprintln!("LookPlox initial index failed: {error}");
        }
      }
    });
  }

  Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_global_shortcut::Builder::new().build())
    .setup(|app| {
      setup_app(app.handle())?;
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![search_files, open_path])
    .run(tauri::generate_context!())
    .expect("error while running LookPlox");
}
