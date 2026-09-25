mod platform;

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use serde::{Deserialize, Serialize};
use rusqlite::{params, Connection};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{
  atomic::{AtomicBool, AtomicUsize, Ordering},
  Arc, Mutex,
};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tantivy::collector::{DocSetCollector, TopDocs};
use tantivy::doc;
use tantivy::query::{AllQuery, BooleanQuery, Query, TermQuery};
use tantivy::schema::{
  Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, Term, TantivyDocument};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use walkdir::WalkDir;

use platform::{
  application_extension_trimmed, bytes_to_data_url, configure_main_window,
  create_application_preview, discover_applications, is_application_container,
  is_application_path,
  is_inside_application_container, open_path as platform_open_path, should_walk_entry,
  ApplicationCache, ApplicationEntry,
};

pub struct AppState {
  pub engine: Arc<SearchEngine>,
  pub settings_db: Arc<Mutex<Connection>>,
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
  pub preview: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
  pub results: Vec<SearchResult>,
  pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
  pub result_limit: usize,
  pub show_paths: bool,
  pub theme: String,
  pub hide_on_blur: bool,
  pub preview_images: bool,
  pub preview_applications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageLocations {
  pub settings_db_path: String,
  pub settings_db_dir: String,
  pub index_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageConfig {
  settings_db_path: String,
  index_path: String,
}

const DEFAULT_RESULT_LIMIT: usize = 12;
const DEFAULT_SHOW_PATHS: bool = true;
const DEFAULT_THEME: &str = "light";
const DEFAULT_HIDE_ON_BLUR: bool = true;
const DEFAULT_PREVIEW_IMAGES: bool = true;
const DEFAULT_PREVIEW_APPLICATIONS: bool = true;

fn default_settings() -> AppSettings {
  AppSettings {
    result_limit: DEFAULT_RESULT_LIMIT,
    show_paths: DEFAULT_SHOW_PATHS,
    theme: DEFAULT_THEME.to_string(),
    hide_on_blur: DEFAULT_HIDE_ON_BLUR,
    preview_images: DEFAULT_PREVIEW_IMAGES,
    preview_applications: DEFAULT_PREVIEW_APPLICATIONS,
  }
}

fn validate_settings(settings: &AppSettings) -> Result<(), String> {
  if ![6, 12, 24, 50].contains(&settings.result_limit) {
    return Err("Invalid result limit.".into());
  }

  if settings.theme != "system" && settings.theme != "light" && settings.theme != "dark" {
    return Err("Invalid theme.".into());
  }

  Ok(())
}

fn storage_config_path(app: &AppHandle) -> Result<PathBuf, String> {
  app.path()
    .app_data_dir()
    .map(|dir| dir.join("storage-config.json"))
    .map_err(|error| error.to_string())
}

fn default_storage_config(app: &AppHandle) -> Result<StorageConfig, String> {
  let data_dir = app.path().app_data_dir().map_err(|error| error.to_string())?;

  Ok(StorageConfig {
    settings_db_path: data_dir.join("settings.sqlite3").to_string_lossy().into_owned(),
    index_path: data_dir.join("index").to_string_lossy().into_owned(),
  })
}

fn read_storage_config(app: &AppHandle) -> Result<StorageConfig, String> {
  let path = storage_config_path(app)?;

  if !path.exists() {
    return default_storage_config(app);
  }

  let raw = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
  let config: StorageConfig = serde_json::from_str(&raw)
    .map_err(|error| format!("Could not read storage configuration: {error}"))?;

  if config.settings_db_path.trim().is_empty() || config.index_path.trim().is_empty() {
    return Err("Storage configuration contains an empty path.".into());
  }

  Ok(config)
}

fn save_storage_config(app: &AppHandle, config: &StorageConfig) -> Result<(), String> {
  let path = storage_config_path(app)?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  }

  let json = serde_json::to_string_pretty(config)
    .map_err(|error| format!("Could not serialize storage configuration: {error}"))?;
  let temp_path = path.with_extension("json.tmp");

  std::fs::write(&temp_path, json).map_err(|error| error.to_string())?;
  std::fs::rename(&temp_path, &path).map_err(|error| error.to_string())?;
  Ok(())
}

fn storage_locations_from_config(config: &StorageConfig) -> Result<StorageLocations, String> {
  let db_path = PathBuf::from(&config.settings_db_path);
  let index_path = PathBuf::from(&config.index_path);

  let settings_db_dir = db_path
    .parent()
    .ok_or_else(|| "Settings database path has no parent directory.".to_string())?;

  Ok(StorageLocations {
    settings_db_path: db_path.to_string_lossy().into_owned(),
    settings_db_dir: settings_db_dir.to_string_lossy().into_owned(),
    index_path: index_path.to_string_lossy().into_owned(),
  })
}

fn settings_db_path(app: &AppHandle) -> Result<PathBuf, String> {
  Ok(PathBuf::from(read_storage_config(app)?.settings_db_path))
}

fn open_settings_db_at(path: &Path) -> Result<Connection, String> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
  }

  let connection = Connection::open(path).map_err(|error| error.to_string())?;

  connection
    .execute_batch(
      "CREATE TABLE IF NOT EXISTS settings (
         id INTEGER PRIMARY KEY CHECK (id = 1),
         result_limit INTEGER NOT NULL,
         show_paths INTEGER NOT NULL,
         theme TEXT NOT NULL,
         hide_on_blur INTEGER NOT NULL,
         preview_images INTEGER NOT NULL DEFAULT 1,
         preview_applications INTEGER NOT NULL DEFAULT 1
       );
       INSERT OR IGNORE INTO settings
         (id, result_limit, show_paths, theme, hide_on_blur, preview_images, preview_applications)
       VALUES (1, 12, 1, 'light', 1, 1, 1);",
    )
    .map_err(|error| error.to_string())?;

  // Migrate settings databases created by older LookPlox versions.
  let existing_columns = {
    let mut statement = connection
      .prepare("PRAGMA table_info(settings)")
      .map_err(|error| error.to_string())?;
    let rows = statement
      .query_map([], |row| row.get::<_, String>(1))
      .map_err(|error| error.to_string())?;

    rows.collect::<Result<Vec<_>, _>>()
      .map_err(|error| error.to_string())?
  };

  if !existing_columns.iter().any(|column| column == "preview_images") {
    connection
      .execute(
        "ALTER TABLE settings ADD COLUMN preview_images INTEGER NOT NULL DEFAULT 1",
        [],
      )
      .map_err(|error| error.to_string())?;
  }

  if !existing_columns.iter().any(|column| column == "preview_applications") {
    connection
      .execute(
        "ALTER TABLE settings ADD COLUMN preview_applications INTEGER NOT NULL DEFAULT 1",
        [],
      )
      .map_err(|error| error.to_string())?;
  }

  Ok(connection)
}

fn open_settings_db(app: &AppHandle) -> Result<Connection, String> {
  open_settings_db_at(&settings_db_path(app)?)
}

fn read_settings(connection: &Connection) -> Result<AppSettings, String> {
  let settings = connection
    .query_row(
      "SELECT result_limit, show_paths, theme, hide_on_blur, preview_images, preview_applications FROM settings WHERE id = 1",
      [],
      |row| {
        Ok(AppSettings {
          result_limit: row.get::<_, i64>(0)? as usize,
          show_paths: row.get::<_, i64>(1)? != 0,
          theme: row.get(2)?,
          hide_on_blur: row.get::<_, i64>(3)? != 0,
          preview_images: row.get::<_, i64>(4)? != 0,
          preview_applications: row.get::<_, i64>(5)? != 0,
        })
      },
    )
    .map_err(|error| error.to_string())?;

  validate_settings(&settings)?;
  Ok(settings)
}

fn write_settings(connection: &Connection, settings: &AppSettings) -> Result<(), String> {
  validate_settings(settings)?;

  connection
    .execute(
      "INSERT INTO settings
         (id, result_limit, show_paths, theme, hide_on_blur, preview_images, preview_applications)
       VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
       ON CONFLICT(id) DO UPDATE SET
         result_limit = excluded.result_limit,
         show_paths = excluded.show_paths,
         theme = excluded.theme,
         hide_on_blur = excluded.hide_on_blur,
         preview_images = excluded.preview_images,
         preview_applications = excluded.preview_applications",
      params![
        settings.result_limit as i64,
        if settings.show_paths { 1i64 } else { 0i64 },
        settings.theme.as_str(),
        if settings.hide_on_blur { 1i64 } else { 0i64 },
        if settings.preview_images { 1i64 } else { 0i64 },
        if settings.preview_applications { 1i64 } else { 0i64 },
      ],
    )
    .map_err(|error| error.to_string())?;

  Ok(())
}

#[derive(Debug, Serialize)]
pub struct SetupState {
  pub initialized: bool,
  pub roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuggestedFolder {
  pub id: String,
  pub name: String,
  pub path: String,
}

#[tauri::command]
fn get_suggested_folders() -> Vec<SuggestedFolder> {
  let candidates = [
    ("desktop", "Desktop", dirs::desktop_dir()),
    ("documents", "Documents", dirs::document_dir()),
    ("downloads", "Downloads", dirs::download_dir()),
    ("pictures", "Pictures", dirs::picture_dir()),
    ("videos", "Videos", dirs::video_dir()),
  ];

  let mut seen = HashSet::<PathBuf>::new();

  candidates
    .into_iter()
    .filter_map(|(id, name, path)| {
      let path = path?;
      if !path.is_dir() || !seen.insert(path.clone()) {
        return None;
      }

      Some(SuggestedFolder {
        id: id.to_owned(),
        name: name.to_owned(),
        path: path.to_string_lossy().into_owned(),
      })
    })
    .collect()
}

#[derive(Debug, Serialize)]
pub struct IndexingStatus {
  pub running: bool,
  pub indexed: usize,
  pub error: Option<String>,
}

const INDEX_VERSION: &str = "5";

pub struct SearchEngine {
  index: Index,
  reader: IndexReader,
  writer: Mutex<IndexWriter>,
  name_field: Field,
  path_field: Field,
  is_dir_field: Field,
  application_cache: Mutex<ApplicationCache>,
}

fn edit_distance(left: &str, right: &str) -> usize {
  let right_chars: Vec<char> = right.chars().collect();
  let mut previous: Vec<usize> = (0..=right_chars.len()).collect();

  for (row, left_char) in left.chars().enumerate() {
    let mut current = vec![row + 1; right_chars.len() + 1];

    for (column, right_char) in right_chars.iter().enumerate() {
      let substitution = previous[column] + if left_char == *right_char { 0 } else { 1 };
      let insertion = current[column] + 1;
      let deletion = previous[column + 1] + 1;

      current[column + 1] = substitution.min(insertion).min(deletion);
    }

    previous = current;
  }

  previous[right_chars.len()]
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
      application_cache: Mutex::new(ApplicationCache::new()),
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
    let is_dir = metadata.is_dir() && !is_application_container(path);

    let writer = self
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

    if raw_path.is_empty() && normalized_path.is_empty() {
      return Ok(());
    }

    let prefix = if normalized_path.is_empty() {
      raw_path.clone()
    } else {
      format!("{}{}", normalized_path, std::path::MAIN_SEPARATOR)
    };

    // Do not build a regex from filesystem paths. Paths can contain regex
    // metacharacters, and Tantivy's regex parser rejects some escaped forms.
    // Instead, inspect the stored path values and remove exact descendants.
    let searcher = self.reader.searcher();
    let matching_docs = searcher
      .search(&AllQuery, &DocSetCollector)
      .map_err(|error| tantivy::TantivyError::InvalidArgument(error.to_string()))?;

    let mut paths_to_remove = HashSet::<String>::with_capacity(matching_docs.len() + 2);

    if !raw_path.is_empty() {
      paths_to_remove.insert(raw_path);
    }

    if !normalized_path.is_empty() {
      paths_to_remove.insert(normalized_path);
    }

    for address in matching_docs {
      let document: TantivyDocument = searcher.doc::<TantivyDocument>(address)?;

      let Some(value) = document
        .get_first(self.path_field)
        .and_then(|value| value.as_str())
      else {
        continue;
      };

      if value == prefix.trim_end_matches(['/', '\\'])
        || value.starts_with(&prefix)
      {
        paths_to_remove.insert(value.to_owned());
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

  fn application_entries(&self) -> Vec<ApplicationEntry> {
    let mut cache = match self.application_cache.lock() {
      Ok(value) => value,
      Err(_) => return discover_applications(),
    };

    if !cache.is_fresh() {
      cache.entries = discover_applications();
      cache.refreshed_at = Some(Instant::now());
    }

    cache.entries.clone()
  }

  pub fn search(
    &self,
    query: &str,
    limit: usize,
    applications_only: bool,
  ) -> Result<SearchResponse, String> {
    let normalized = query.trim().to_lowercase();
    if normalized.is_empty() {
      return Ok(SearchResponse {
        results: Vec::new(),
        suggestion: None,
      });
    }

    if applications_only {
      let requested_limit = limit.clamp(1, 50);
      let entries = self.application_entries();

      let mut matched = entries
        .iter()
        .filter(|entry| {
          let comparable = application_extension_trimmed(&entry.name).to_lowercase();
          comparable.contains(&normalized) || entry.name.to_lowercase().contains(&normalized)
        })
        .map(|entry| SearchResult {
          name: entry.name.clone(),
          path: entry.path.to_string_lossy().into_owned(),
          is_dir: false,
          preview: None,
        })
        .collect::<Vec<_>>();

      matched.sort_by(|left, right| {
        let left_name = application_extension_trimmed(&left.name).to_lowercase();
        let right_name = application_extension_trimmed(&right.name).to_lowercase();

        let left_prefix = left_name.starts_with(&normalized);
        let right_prefix = right_name.starts_with(&normalized);

        right_prefix
          .cmp(&left_prefix)
          .then_with(|| left_name.cmp(&right_name))
          .then_with(|| left.path.cmp(&right.path))
      });

      matched.truncate(requested_limit);

      let suggestion = if matched.is_empty() {
        let mut best: Option<(usize, usize, String)> = None;

        for entry in &entries {
          let comparable = application_extension_trimmed(&entry.name).to_lowercase();
          let distance = edit_distance(&normalized, &comparable);
          let length_gap =
            comparable.chars().count().abs_diff(normalized.chars().count());
          let max_distance = match normalized.chars().count() {
            0..=4 => 1,
            5..=7 => 2,
            _ => (normalized.chars().count() / 3).max(2),
          };

          if distance > max_distance {
            continue;
          }

          let candidate = (distance, length_gap, entry.name.clone());

          if best
            .as_ref()
            .map(|current| {
              candidate.0 < current.0
                || (candidate.0 == current.0 && candidate.1 < current.1)
            })
            .unwrap_or(true)
          {
            best = Some(candidate);
          }
        }

        best.map(|(_, _, name)| name)
      } else {
        None
      };

      return Ok(SearchResponse {
        results: matched,
        suggestion,
      });
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
      return Ok(SearchResponse {
        results: Vec::new(),
        suggestion: None,
      });
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

    // Application filtering is done against the stored filesystem path below.
    // Do not use a Tantivy regex query here: the path field is intentionally
    // optimized for exact path deletion, while the application check is a
    // platform-specific filesystem rule.
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

      if applications_only && !is_application_path(Path::new(&path)) {
        continue;
      }

      if !name.is_empty()
        && !path.is_empty()
        && name.to_lowercase().contains(&normalized)
      {
        results.push(SearchResult {
          name,
          path,
          is_dir,
          preview: None,
        });

        if results.len() >= requested_limit {
          break;
        }
      }
    }

    let suggestion = if results.is_empty() {
      self.suggest(&normalized, applications_only)?
    } else {
      None
    };

    Ok(SearchResponse {
      results,
      suggestion,
    })
  }

  fn suggest(&self, query: &str, applications_only: bool) -> Result<Option<String>, String> {
    let normalized = query.trim().to_lowercase();
    if normalized.chars().count() < 2 {
      return Ok(None);
    }

    let mut analyzer = self
      .index
      .tokenizers()
      .get("filename_ngram")
      .ok_or_else(|| "Search tokenizer is unavailable.".to_string())?;

    let mut stream = analyzer.token_stream(&normalized);
    let mut tokens = Vec::<String>::new();

    stream.process(&mut |token| {
      if token.text.chars().count() >= 2
        && token.text.chars().count() <= 4
        && !tokens.iter().any(|item| item == &token.text)
      {
        tokens.push(token.text.clone());
      }
    });

    if tokens.is_empty() {
      return Ok(None);
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

    let parsed: Box<dyn Query> = Box::new(BooleanQuery::union(clauses));

    let searcher = self.reader.searcher();
    let candidates = searcher
      .search(&parsed, &TopDocs::with_limit(200).order_by_score())
      .map_err(|error| error.to_string())?;

    let max_distance = match normalized.chars().count() {
      0..=4 => 1,
      5..=7 => 2,
      _ => (normalized.chars().count() / 3).max(2),
    };

    let mut best: Option<(usize, usize, String)> = None;

    for (_, address) in candidates {
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

      if name.is_empty() || path.is_empty() {
        continue;
      }

      if applications_only && !is_application_path(Path::new(&path)) {
        continue;
      }

      let comparable = application_extension_trimmed(&name).to_lowercase();
      let distance = edit_distance(&normalized, &comparable);

      if distance > max_distance {
        continue;
      }

      let length_gap = comparable.chars().count().abs_diff(normalized.chars().count());
      let candidate = (distance, length_gap, name);

      if best
        .as_ref()
        .map(|current| candidate.0 < current.0 || (candidate.0 == current.0 && candidate.1 < current.1))
        .unwrap_or(true)
      {
        best = Some(candidate);
      }
    }

    Ok(best.map(|(_, _, name)| name))
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

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<(), String> {
  if !source.is_dir() {
    return Err(format!("Index source is not a directory: {}", source.display()));
  }

  std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;

  for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
    let entry = entry.map_err(|error| error.to_string())?;
    let source_path = entry.path();
    let destination_path = destination.join(entry.file_name());
    let file_type = entry.file_type().map_err(|error| error.to_string())?;

    if file_type.is_dir() {
      copy_directory_recursive(&source_path, &destination_path)?;
    } else if file_type.is_file() {
      std::fs::copy(&source_path, &destination_path).map_err(|error| error.to_string())?;
    } else {
      return Err(format!("Unsupported item in index directory: {}", source_path.display()));
    }
  }

  Ok(())
}

fn is_directory_empty(path: &Path) -> Result<bool, String> {
  let mut entries = std::fs::read_dir(path).map_err(|error| error.to_string())?;
  Ok(entries.next().is_none())
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
      if is_inside_application_container(&path) {
        continue;
      }

      if let Err(error) = engine.upsert_path(&path) {
        eprintln!("LookPlox failed to index folder {:?}: {error}", path);
      } else {
        changed = true;
      }

      if is_application_container(&path) {
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
fn get_storage_locations(app: AppHandle) -> Result<StorageLocations, String> {
  let config = read_storage_config(&app)?;
  storage_locations_from_config(&config)
}

#[tauri::command]
fn change_storage_locations(
  app: AppHandle,
  state: State<'_, AppState>,
  settings_db_dir: String,
  index_dir: String,
) -> Result<(), String> {
  if state.indexing.running.load(Ordering::SeqCst) {
    return Err("Stop indexing before changing storage locations.".into());
  }

  let current = read_storage_config(&app)?;
  let current_db = PathBuf::from(&current.settings_db_path);
  let current_index = PathBuf::from(&current.index_path);
  let new_db_dir = PathBuf::from(settings_db_dir);
  let new_index = PathBuf::from(index_dir);

  if !new_db_dir.is_dir() {
    return Err("The selected settings database folder is not available.".into());
  }

  if !new_index.is_dir() && new_index.exists() {
    return Err("The selected search index location is not a folder.".into());
  }

  let new_db = new_db_dir.join("settings.sqlite3");

  if current_db == new_db && current_index == new_index {
    return Ok(());
  }

  if current_index != new_index
    && (new_index.starts_with(&current_index) || current_index.starts_with(&new_index))
  {
    return Err(
      "The new search index folder must not contain or be inside the current index folder."
        .into(),
    );
  }

  let current_settings = {
    let connection = state
      .settings_db
      .lock()
      .map_err(|_| "Settings database is unavailable.".to_string())?;

    read_settings(&connection)?
  };

  // Create the new settings database before touching the search index.
  // The current database remains intact until the storage configuration is committed.
  if current_db != new_db {
    let new_connection = open_settings_db_at(&new_db)?;
    write_settings(&new_connection, &current_settings)?;
  }

  let mut index_was_renamed = false;
  let mut index_was_copied = false;

  if current_index != new_index {
    if new_index.exists() {
      if !is_directory_empty(&new_index)? {
        return Err(
          "The selected search index folder must be empty before LookPlox can use it.".into(),
        );
      }
      std::fs::remove_dir(&new_index).map_err(|error| error.to_string())?;
    } else if let Some(parent) = new_index.parent() {
      std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    if current_index.exists() {
      match std::fs::rename(&current_index, &new_index) {
        Ok(()) => {
          index_was_renamed = true;
        }
        Err(_) => {
          if let Err(error) = copy_directory_recursive(&current_index, &new_index) {
            let _ = std::fs::remove_dir_all(&new_index);
            return Err(error);
          }
          index_was_copied = true;
        }
      }
    } else {
      std::fs::create_dir_all(&new_index).map_err(|error| error.to_string())?;
    }
  }

  let next_config = StorageConfig {
    settings_db_path: new_db.to_string_lossy().into_owned(),
    index_path: new_index.to_string_lossy().into_owned(),
  };

  if let Err(error) = save_storage_config(&app, &next_config) {
    if index_was_renamed {
      if let Err(rollback_error) = std::fs::rename(&new_index, &current_index) {
        eprintln!("LookPlox could not roll back the index move: {rollback_error}");
      }
    } else if index_was_copied {
      let _ = std::fs::remove_dir_all(&new_index);
    }

    return Err(error);
  }

  if index_was_copied && current_index.exists() {
    if let Err(error) = std::fs::remove_dir_all(&current_index) {
      eprintln!("LookPlox could not remove the old search index after copying it: {error}");
    }
  }

  if current_db != new_db && current_db.exists() {
    if let Err(error) = std::fs::remove_file(&current_db) {
      eprintln!("LookPlox could not remove the old settings database: {error}");
    }
  }

  app.restart();
}
#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
  let connection = state
    .settings_db
    .lock()
    .map_err(|_| "Settings database is unavailable.".to_string())?;

  read_settings(&connection)
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: AppSettings) -> Result<(), String> {
  let connection = state
    .settings_db
    .lock()
    .map_err(|_| "Settings database is unavailable.".to_string())?;

  write_settings(&connection, &settings)
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


fn image_mime_type(path: &Path) -> Option<&'static str> {
  match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
    "png" => Some("image/png"),
    "jpg" | "jpeg" => Some("image/jpeg"),
    "gif" => Some("image/gif"),
    "webp" => Some("image/webp"),
    "bmp" => Some("image/bmp"),
    "svg" => Some("image/svg+xml"),
    "avif" => Some("image/avif"),
    "ico" => Some("image/x-icon"),
    _ => None,
  }
}

fn create_app_preview(path: &Path) -> Result<Option<String>, String> {
  create_application_preview(path)
}

fn get_file_preview(path: &Path) -> Result<Option<String>, String> {
  if let Some(mime) = image_mime_type(path) {
    const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;

    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_PREVIEW_BYTES {
      return Ok(None);
    }

    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    return Ok(Some(bytes_to_data_url(&bytes, mime)));
  }

  create_app_preview(path)
}

fn app_preview_cache_path(cache_dir: &Path, path: &Path) -> PathBuf {
  let mut hasher = DefaultHasher::new();
  path.to_string_lossy().hash(&mut hasher);
  cache_dir.join(format!("{:016x}.txt", hasher.finish()))
}

fn app_preview_cache_stamp(path: &Path) -> u128 {
  std::fs::metadata(path)
    .ok()
    .and_then(|metadata| metadata.modified().ok())
    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
    .map(|duration| duration.as_nanos())
    .unwrap_or(0)
}

fn get_cached_app_preview(path: &Path, cache_dir: &Path) -> Result<Option<String>, String> {
  let cache_path = app_preview_cache_path(cache_dir, path);
  let stamp = app_preview_cache_stamp(path);

  if let Ok(cached) = std::fs::read_to_string(&cache_path) {
    if let Some((cached_stamp, preview)) = cached.split_once('\n') {
      if cached_stamp.parse::<u128>().ok() == Some(stamp)
        && preview.starts_with("data:image/png;base64,")
      {
        return Ok(Some(preview.to_owned()));
      }
    }
  }

  let Some(preview) = create_app_preview(path)? else {
    return Ok(None);
  };

  // The cache is disposable. A plain write is intentional here: if the app
  // exits during the write, the invalid entry is simply regenerated next time.
  let cache_contents = format!("{stamp}\n{preview}");
  std::fs::write(&cache_path, cache_contents).map_err(|error| error.to_string())?;

  Ok(Some(preview))
}

fn get_file_previews_sync(
  paths: Vec<String>,
  preview_images: bool,
  preview_applications: bool,
  app_data_dir: PathBuf,
) -> Result<HashMap<String, String>, String> {
  let mut previews = HashMap::new();

  let app_cache_dir = if preview_applications {
    let cache_dir = app_data_dir.join("preview-cache");
    std::fs::create_dir_all(&cache_dir).map_err(|error| error.to_string())?;
    Some(cache_dir)
  } else {
    None
  };

  for path_string in paths {
    let path = PathBuf::from(&path_string);

    let should_preview_image = preview_images && image_mime_type(&path).is_some();
    let should_preview_app = preview_applications && is_application_path(&path);

    if !should_preview_image && !should_preview_app {
      continue;
    }

    let result = if should_preview_image {
      get_file_preview(&path)
    } else if let Some(cache_dir) = app_cache_dir.as_deref() {
      get_cached_app_preview(&path, cache_dir)
    } else {
      get_file_preview(&path)
    };

    match result {
      Ok(Some(preview)) => {
        previews.insert(path_string, preview);
      }
      Ok(None) => {}
      Err(error) => {
        eprintln!("LookPlox could not create a preview for {:?}: {error}", path);
      }
    }
  }

  Ok(previews)
}

#[tauri::command]
async fn get_file_previews(
  app: AppHandle,
  paths: Vec<String>,
  preview_images: bool,
  preview_applications: bool,
) -> Result<HashMap<String, String>, String> {
  let app_data_dir = app
    .path()
    .app_data_dir()
    .map_err(|error| error.to_string())?;

  tauri::async_runtime::spawn_blocking(move || {
    get_file_previews_sync(
      paths,
      preview_images,
      preview_applications,
      app_data_dir,
    )
  })
  .await
  .map_err(|error| format!("Preview worker failed: {error}"))?
}

#[tauri::command]
fn search_files(
  state: State<'_, AppState>,
  query: String,
  limit: Option<usize>,
  applications_only: Option<bool>,
) -> Result<SearchResponse, String> {
  state.engine.search(
    &query,
    limit.unwrap_or(12),
    applications_only.unwrap_or(false),
  )
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
  let path_ref = Path::new(&path);

  if !path_ref.exists() {
    return Err(format!("Path does not exist: {path}"));
  }

  platform_open_path(path_ref)
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

        let storage_config = read_storage_config(app.handle())?;
        save_storage_config(app.handle(), &storage_config)?;

        let settings_db = Arc::new(Mutex::new(open_settings_db(app.handle())?));

        let index_dir = PathBuf::from(&storage_config.index_path);

        if !is_index_current(app.handle())? && index_dir.exists() {
          std::fs::remove_dir_all(&index_dir)?;
        }

        let engine = Arc::new(SearchEngine::open(&index_dir)?);

        let roots = read_roots(app.handle())?;
        let is_initialized =
          marker_path(app.handle())?.exists()
            && !roots.is_empty()
            && is_index_current(app.handle())?;

        if let Some(window) = app.get_webview_window("main") {
          configure_main_window(&window);
        }

        initialized.store(is_initialized, Ordering::SeqCst);

        let active_roots = Arc::new(Mutex::new(roots.clone()));
        let watched_roots = Arc::new(Mutex::new(HashSet::new()));

        app.manage(AppState {
          engine: Arc::clone(&engine),
          settings_db: Arc::clone(&settings_db),
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
      get_settings,
      get_suggested_folders,
      save_settings,
      get_storage_locations,
      change_storage_locations,
      get_setup_state,
      get_indexing_status,
      start_indexing,
      cancel_indexing,
      add_index_root,
      remove_index_root,
      search_files,
      get_file_previews,
      open_path
    ])
    .run(tauri::generate_context!())
    .expect("error while running LookPlox");
}


#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn edit_distance_handles_exact_and_single_edit() {
    assert_eq!(edit_distance("safari", "safari"), 0);
    assert_eq!(edit_distance("safri", "safari"), 1);
    assert_eq!(edit_distance("finder", "findr"), 1);
  }

  #[test]
  fn application_extension_is_removed_for_display_matching() {
    assert_eq!(application_extension_trimmed("Safari.app"), "Safari");
    assert_eq!(application_extension_trimmed("Tool.EXE"), "Tool");
    assert_eq!(application_extension_trimmed("sample.txt"), "sample.txt");
  }

  #[test]
  fn application_names_match_without_bundle_suffix() {
    let name = application_extension_trimmed("Safari.app").to_lowercase();
    assert!(name.contains("safari"));
    assert!(!name.contains(".app"));
  }
}
