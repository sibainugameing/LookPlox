<script lang="ts">
  import { onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { LogicalSize } from "@tauri-apps/api/dpi";
  import { open } from "@tauri-apps/plugin-dialog";

  type SearchResult = {
    name: string;
    path: string;
    is_dir: boolean;
    preview?: string | null;
  };

  type SearchResponse = {
    results: SearchResult[];
    suggestion: string | null;
  };

  type SetupState = {
    initialized: boolean;
    roots: string[];
  };

  type IndexingStatus = {
    running: boolean;
    indexed: number;
    error: string | null;
  };

  type Theme = "system" | "light" | "dark";

  type Settings = {
    resultLimit: number;
    showPaths: boolean;
    theme: Theme;
    hideOnBlur: boolean;
    previewImages: boolean;
    previewApplications: boolean;
  };

  type StorageLocations = {
    settingsDbPath: string;
    settingsDbDir: string;
    indexPath: string;
  };

  type CommandItem = {
    command: string;
    description: string;
  };

  const LEGACY_SETTINGS_KEY = "lookplox.settings";
  const DEFAULT_SETTINGS: Settings = {
    resultLimit: 12,
    showPaths: true,
    theme: "light",
    hideOnBlur: true,
    previewImages: true,
    previewApplications: true,
  };

  const APPLICATION_SEARCH_PREFIX = "@";

  const COMMANDS: CommandItem[] = [
    { command: "/config", description: "Open LookPlox settings" },
    { command: "/add-folder", description: "Add a folder to the search index" },
    { command: "/help", description: "Show available commands" },
  ];

  let query = "";
  let results: SearchResult[] = [];
  let searchSuggestion = "";
  let commandMatches: CommandItem[] = [];
  let selected = 0;
  let input: HTMLInputElement;
  let searching = false;
  let requestId = 0;
  let viewMode: "search" | "config" | "help" = "search";

  let setupMode = true;
  let roots: string[] = [];
  let indexing = false;
  let cancelRequested = false;
  let indexedCount = 0;
  let setupError = "";
  let indexMessage = "";
  let indexPoll: number | undefined;
  let folderPickerOpen = false;

  let settings: Settings = { ...DEFAULT_SETTINGS };
  let storageLocations: StorageLocations | null = null;
  let storageChanging = false;
  let storageError = "";

  const windowHandle = getCurrentWindow();

  function normalizeSettings(parsed: Partial<Settings>): Settings {
    return {
      resultLimit:
        typeof parsed.resultLimit === "number" && [6, 12, 24, 50].includes(parsed.resultLimit)
          ? parsed.resultLimit
          : DEFAULT_SETTINGS.resultLimit,
      showPaths:
        typeof parsed.showPaths === "boolean"
          ? parsed.showPaths
          : DEFAULT_SETTINGS.showPaths,
      theme:
        parsed.theme === "light" || parsed.theme === "dark" || parsed.theme === "system"
          ? parsed.theme
          : DEFAULT_SETTINGS.theme,
      hideOnBlur:
        typeof parsed.hideOnBlur === "boolean"
          ? parsed.hideOnBlur
          : DEFAULT_SETTINGS.hideOnBlur,
      previewImages:
        typeof parsed.previewImages === "boolean"
          ? parsed.previewImages
          : DEFAULT_SETTINGS.previewImages,
      previewApplications:
        typeof parsed.previewApplications === "boolean"
          ? parsed.previewApplications
          : DEFAULT_SETTINGS.previewApplications,
    };
  }

  async function loadSettings() {
    try {
      const stored = await invoke<Settings>("get_settings");
      settings = normalizeSettings(stored);

      const legacyRaw = localStorage.getItem(LEGACY_SETTINGS_KEY);
      if (legacyRaw) {
        try {
          const legacyParsed = JSON.parse(legacyRaw) as Partial<Settings>;
          const migrated = normalizeSettings(legacyParsed);

          await invoke("save_settings", { settings: migrated });
          settings = migrated;
        } catch {
          // Ignore an invalid legacy value and keep the SQLite settings.
        }

        localStorage.removeItem(LEGACY_SETTINGS_KEY);
      }
    } catch (error) {
      console.error("LookPlox settings could not be loaded from SQLite:", error);
      settings = { ...DEFAULT_SETTINGS };
    }

    applyTheme();
  }

  let settingsSaveQueue: Promise<void> = Promise.resolve();

  function saveSettings() {
    applyTheme();

    const snapshot = { ...settings };
    settingsSaveQueue = settingsSaveQueue
      .then(async () => {
        await invoke("save_settings", { settings: snapshot });
      })
      .catch((error) => {
        console.error("LookPlox settings could not be saved to SQLite:", error);
      });
  }

  async function refreshResultPreviews(
    items: SearchResult[] = results,
    requestToken = requestId,
  ) {
    if (items.length === 0) {
      return;
    }

    if (!settings.previewImages && !settings.previewApplications) {
      results = items.map((item) => ({ ...item, preview: null }));
      return;
    }

    // Only load previews for the currently visible portion first. Each item
    // is fetched independently so fast/cache-hit icons can appear immediately
    // instead of waiting for the slowest preview in the whole result set.
    const previewItems = items.slice(0, 8);
    const loadPreview = async (item: SearchResult) => {
      try {
        const previews = await invoke<Record<string, string>>("get_file_previews", {
          paths: [item.path],
          previewImages: settings.previewImages,
          previewApplications: settings.previewApplications,
        });

        if (requestToken !== requestId) {
          return;
        }

        const preview = previews[item.path] ?? null;
        results = results.map((current) =>
          current.path === item.path ? { ...current, preview } : current,
        );
      } catch (error) {
        console.error("LookPlox preview could not be loaded:", error);
        if (requestToken !== requestId) {
          return;
        }

        results = results.map((current) =>
          current.path === item.path ? { ...current, preview: null } : current,
        );
      }
    };

    // Keep native preview generation bounded while allowing previews to
    // appear progressively as individual requests finish.
    for (let index = 0; index < previewItems.length; index += 4) {
      await Promise.all(previewItems.slice(index, index + 4).map(loadPreview));
      if (requestToken !== requestId) {
        return;
      }
    }
  }

  function updateSettings(patch: Partial<Settings>) {
    settings = { ...settings, ...patch };
    saveSettings();

    if ("previewImages" in patch || "previewApplications" in patch) {
      void refreshResultPreviews();
    }
  }

  async function loadStorageLocations() {
    try {
      storageLocations = await invoke<StorageLocations>("get_storage_locations");
      storageError = "";
    } catch (error) {
      storageError = "Could not load storage locations: " + String(error);
    }
  }

  async function chooseStorageLocation(kind: "settings-db" | "index") {
    if (storageChanging || indexing || !storageLocations) {
      return;
    }

    storageError = "";
    folderPickerOpen = true;

    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        recursive: true,
        title:
          kind === "settings-db"
            ? "Choose the LookPlox settings database folder"
            : "Choose the LookPlox search index folder",
      });

      if (typeof selectedPath !== "string") {
        return;
      }

      storageChanging = true;
      indexMessage = "Moving LookPlox data and restarting…";

      await invoke("change_storage_locations", {
        settingsDbDir:
          kind === "settings-db" ? selectedPath : storageLocations.settingsDbDir,
        indexDir: kind === "index" ? selectedPath : storageLocations.indexPath,
      });
    } catch (error) {
      storageChanging = false;
      indexMessage = "";
      storageError = String(error);
    } finally {
      folderPickerOpen = false;
    }
  }

  function applyTheme() {
    if (typeof document === "undefined") {
      return;
    }

    if (settings.theme === "system") {
      document.documentElement.removeAttribute("data-theme");
    } else {
      document.documentElement.dataset.theme = settings.theme;
    }
  }

  async function resizeSearchWindow(resultCount = 0, hasSuggestion = false) {
    const visibleResults = Math.min(Math.max(resultCount, 1), 8);
    const height =
      resultCount === 0
        ? hasSuggestion
          ? 152
          : 104
        : 12 + 68 + 10 + 16 + visibleResults * 56 + 10;

    await windowHandle.setSize(new LogicalSize(760, height));
  }

  async function resizeSetupWindow() {
    await windowHandle.setSize(new LogicalSize(760, 480));
  }

  async function resizeConfigWindow() {
    await windowHandle.setSize(new LogicalSize(760, 600));
  }

  async function resizeHelpWindow() {
    await windowHandle.setSize(new LogicalSize(760, 360));
  }

  function comparablePath(path: string) {
    const isWindowsDriveRoot = /^[A-Za-z]:[\\/]?$/.test(path);
    if (isWindowsDriveRoot) {
      return path.slice(0, 2) + (path.includes("\\") ? "\\" : "/");
    }

    const trimmed = path.replace(/[\\/]+$/, "");
    return trimmed || path;
  }

  function isSameOrChildPath(candidate: string, parent: string) {
    const candidatePath = comparablePath(candidate);
    const parentPath = comparablePath(parent);

    if (candidatePath === parentPath) {
      return true;
    }

    const separator =
      parentPath.endsWith("\\") || parentPath.endsWith("/")
        ? ""
        : parentPath.includes("\\")
          ? "\\"
          : "/";

    return candidatePath.startsWith(parentPath + separator);
  }

  function addRootCandidate(candidate: string) {
    if (!candidate) {
      return;
    }

    const covered = roots.some((root) => isSameOrChildPath(candidate, root));

    if (!covered) {
      roots = [
        ...roots.filter((root) => !isSameOrChildPath(root, candidate)),
        candidate,
      ];
    }
  }

  async function addFolder() {
    setupError = "";
    folderPickerOpen = true;

    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        recursive: true,
        title: "Select a folder to index",
      });

      if (typeof selectedPath === "string") {
        addRootCandidate(selectedPath);
      }
    } catch (error) {
      setupError = "Could not open the folder picker: " + String(error);
    } finally {
      folderPickerOpen = false;
    }
  }

  function removeRoot(index: number) {
    roots = roots.filter((_, itemIndex) => itemIndex !== index);
  }

  async function startIndexing(rootsToIndex: string[], finishSetupWhenDone = false, message = "Indexing…") {
    if (rootsToIndex.length === 0) {
      setupError = "Add at least one folder to continue.";
      return;
    }

    indexing = true;
    cancelRequested = false;
    indexedCount = 0;
    setupError = "";
    indexMessage = message;

    try {
      await invoke("start_indexing", { roots: rootsToIndex });

      if (indexPoll !== undefined) {
        window.clearInterval(indexPoll);
      }

      indexPoll = window.setInterval(async () => {
        try {
          const status = await invoke<IndexingStatus>("get_indexing_status");
          indexedCount = status.indexed;

          if (!status.running) {
            window.clearInterval(indexPoll);
            indexPoll = undefined;
            indexing = false;

            if (cancelRequested || status.error === "Indexing canceled.") {
              cancelRequested = false;
              indexMessage = "";
              return;
            }

            if (status.error) {
              setupError = status.error;
              indexMessage = "";
              return;
            }

            cancelRequested = false;
            indexMessage = finishSetupWhenDone ? "" : "Index rebuilt";

            if (finishSetupWhenDone) {
              setupMode = false;
            }

            if (finishSetupWhenDone) {
              await resizeSearchWindow();
              await windowHandle.center();
              await windowHandle.setFocus();
              await tick();
              input?.focus();
              input?.select();
            } else {
              window.setTimeout(() => {
                if (!indexing) {
                  indexMessage = "";
                }
              }, 1800);
            }
          }
        } catch (error) {
          if (indexPoll !== undefined) {
            window.clearInterval(indexPoll);
            indexPoll = undefined;
          }
          indexing = false;
          cancelRequested = false;
          setupError = "Indexing status could not be read: " + String(error);
        }
      }, 250);
    } catch (error) {
      indexing = false;
      cancelRequested = false;
      indexMessage = "";
      setupError = String(error);
    }
  }

  async function finishSetup() {
    setupError = "";

    if (roots.length === 0) {
      setupError = "Add at least one folder to continue.";
      return;
    }

    await startIndexing(roots, true, "Building search index…");
  }

  async function cancelIndexing() {
    if (!indexing || cancelRequested) {
      return;
    }

    cancelRequested = true;

    try {
      await invoke("cancel_indexing");
    } catch (error) {
      cancelRequested = false;
      setupError = "Could not cancel indexing: " + String(error);
    }
  }

  async function addIndexFolder() {
    setupError = "";
    indexMessage = "";
    folderPickerOpen = true;

    try {
      const selectedPath = await open({
        directory: true,
        multiple: false,
        recursive: true,
        title: "Add a folder to the LookPlox index",
      });

      if (typeof selectedPath !== "string") {
        return;
      }

      roots = await invoke<string[]>("add_index_root", {
        root: selectedPath,
      });

      await pollIndexing("Indexing folder…");
    } catch (error) {
      setupError = String(error);
      indexMessage = "";
    } finally {
      folderPickerOpen = false;
    }
  }

  async function pollIndexing(message: string) {
    indexing = true;
    cancelRequested = false;
    indexedCount = 0;
    indexMessage = message;

    if (indexPoll !== undefined) {
      window.clearInterval(indexPoll);
    }

    indexPoll = window.setInterval(async () => {
      try {
        const status = await invoke<IndexingStatus>("get_indexing_status");
        indexedCount = status.indexed;

        if (!status.running) {
          window.clearInterval(indexPoll);
          indexPoll = undefined;
          indexing = false;

          if (status.error) {
            setupError = status.error;
            const state = await invoke<SetupState>("get_setup_state");
            roots = state.roots;
            indexMessage = "";
            return;
          }

          indexMessage = "Index updated";
          window.setTimeout(() => {
            indexMessage = "";
          }, 1800);
        }
      } catch (error) {
        if (indexPoll !== undefined) {
          window.clearInterval(indexPoll);
          indexPoll = undefined;
        }
        indexing = false;
        setupError = "Indexing status could not be read: " + String(error);
        indexMessage = "";
      }
    }, 250);
  }

  async function reindexNow() {
    if (indexing) {
      return;
    }

    if (roots.length === 0) {
      setupError = "No folders are configured.";
      return;
    }

    await startIndexing(roots, false, "Rebuilding search index…");
  }

  async function showConfig() {
    viewMode = "config";
    query = "";
    results = [];
    searchSuggestion = "";
    commandMatches = [];
    selected = 0;
    setupError = "";
    storageError = "";
    await loadStorageLocations();
    await resizeConfigWindow();
    await windowHandle.center();
    await windowHandle.setFocus();
  }

  async function showHelp() {
    viewMode = "help";
    query = "";
    results = [];
    searchSuggestion = "";
    commandMatches = [];
    selected = 0;
    await resizeHelpWindow();
    await windowHandle.center();
    await windowHandle.setFocus();
  }

  async function backToSearch() {
    viewMode = "search";
    query = "";
    results = [];
    searchSuggestion = "";
    commandMatches = [];
    selected = 0;
    setupError = "";
    indexMessage = "";
    await resizeSearchWindow(0);
    await windowHandle.setFocus();
    await tick();
    input?.focus();
    input?.select();
  }

  async function executeCommand(command: string) {
    switch (command) {
      case "/config":
        await showConfig();
        break;
      case "/add-folder":
        query = "";
        results = [];
        searchSuggestion = "";
        commandMatches = [];
        await addIndexFolder();
        break;
      case "/help":
        await showHelp();
        break;
    }
  }

  async function removeTrackedFolder(root: string) {
    setupError = "";
    indexMessage = "";

    try {
      roots = await invoke<string[]>("remove_index_root", { root });
      indexMessage = "Folder removed from index";
      window.setTimeout(() => {
        indexMessage = "";
      }, 1800);
    } catch (error) {
      setupError = String(error);
    }
  }

  async function hideSearchWindow() {
    if (setupMode) {
      return;
    }

    if (viewMode !== "search") {
      await backToSearch();
      return;
    }

    query = "";
    results = [];
    searchSuggestion = "";
    commandMatches = [];
    selected = 0;
    await resizeSearchWindow(0);
    await windowHandle.hide();
  }

  function normalizedCommandQuery(value: string) {
    return value.trim().replace(/^／/, "/");
  }

  function normalizedApplicationQuery(value: string) {
    return value.trim().replace(/^＠/, "@");
  }

  function isApplicationQuery(value: string) {
    return normalizedApplicationQuery(value).startsWith(APPLICATION_SEARCH_PREFIX);
  }

  async function search() {
    const currentRequest = ++requestId;
    if (indexMessage && !indexing) {
      indexMessage = "";
    }

    const rawValue = query.trim();
    const value = normalizedCommandQuery(rawValue);
    const applicationQuery = normalizedApplicationQuery(rawValue);

    if (value.startsWith("/")) {
      commandMatches = COMMANDS.filter((item) =>
        item.command.startsWith(value.toLowerCase()),
      );
      results = [];
      searchSuggestion = "";
      selected = Math.min(selected, Math.max(commandMatches.length - 1, 0));
      await resizeSearchWindow(commandMatches.length);
      return;
    }

    commandMatches = [];

    const applicationsOnly = applicationQuery.startsWith(APPLICATION_SEARCH_PREFIX);
    const searchValue = applicationsOnly
      ? applicationQuery.slice(APPLICATION_SEARCH_PREFIX.length).trim()
      : value;

    if (!searchValue) {
      results = [];
      searchSuggestion = "";
      selected = 0;
      await resizeSearchWindow(0);
      return;
    }

    searching = true;

    try {
      const response = await invoke<SearchResponse>("search_files", {
        query: searchValue,
        limit: settings.resultLimit,
        applicationsOnly,
      });

      if (currentRequest === requestId) {
        results = response.results;
        searchSuggestion = response.suggestion ?? "";
        selected = Math.min(selected, Math.max(response.results.length - 1, 0));
        await resizeSearchWindow(response.results.length, Boolean(response.suggestion));
        void refreshResultPreviews(response.results, currentRequest);
      }
    } catch (error) {
      console.error("LookPlox search failed:", error);
      if (currentRequest === requestId) {
        results = [];
        searchSuggestion = "";
      }
    } finally {
      if (currentRequest === requestId) {
        searching = false;
      }
    }
  }

  async function useSearchSuggestion() {
    if (!searchSuggestion) {
      return;
    }

    const currentRequest = ++requestId;
    query = isApplicationQuery(query)
      ? query.trim().replace(/^＠/, "@").slice(0, 1) + searchSuggestion
      : searchSuggestion;
    searchSuggestion = "";
    selected = 0;
    searching = true;

    try {
      const applicationQuery = normalizedApplicationQuery(query);
      const applicationsOnly = applicationQuery.startsWith(APPLICATION_SEARCH_PREFIX);
      const searchValue = applicationsOnly
        ? applicationQuery.slice(APPLICATION_SEARCH_PREFIX.length).trim()
        : query.trim();

      const response = await invoke<SearchResponse>("search_files", {
        query: searchValue,
        limit: settings.resultLimit,
        applicationsOnly,
      });

      if (currentRequest === requestId) {
        results = response.results;
        searchSuggestion = response.suggestion ?? "";
        await resizeSearchWindow(response.results.length, Boolean(response.suggestion));
        void refreshResultPreviews(response.results, currentRequest);
      }
    } catch (error) {
      console.error("LookPlox suggested search failed:", error);
      if (currentRequest === requestId) {
        results = [];
        searchSuggestion = "";
      }
    } finally {
      if (currentRequest === requestId) {
        searching = false;
      }
    }
  }

  async function openResult(result: SearchResult) {
    try {
      await invoke("open_path", { path: result.path });
      await hideSearchWindow();
    } catch (error) {
      console.error("LookPlox failed to open path:", error);
    }
  }

  async function handleKeydown(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      await hideSearchWindow();
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();

      if (viewMode !== "search") {
        return;
      }

      if (isCommandQuery(query)) {
        if (commandMatches.length > 0) {
          selected = Math.min(selected + 1, commandMatches.length - 1);
        }
      } else if (results.length > 0) {
        selected = Math.min(selected + 1, results.length - 1);
      }
      return;
    }

    if (event.key === "ArrowUp") {
      event.preventDefault();

      if (viewMode !== "search") {
        return;
      }

      if (isCommandQuery(query)) {
        if (commandMatches.length > 0) {
          selected = Math.max(selected - 1, 0);
        }
      } else if (results.length > 0) {
        selected = Math.max(selected - 1, 0);
      }
      return;
    }

    if (
      event.key === "Enter" &&
      viewMode === "search" &&
      isCommandQuery(query) &&
      commandMatches[selected]
    ) {
      event.preventDefault();
      await executeCommand(commandMatches[selected].command);
      return;
    }

    if (event.key === "Enter" && results[selected]) {
      event.preventDefault();
      await openResult(results[selected]);
    }
  }

  onMount(() => {
    let unlistenFocus: (() => void) | undefined;

    const initialize = async () => {
      await loadSettings();
      await loadStorageLocations();

      try {
        const state = await invoke<SetupState>("get_setup_state");
        roots = state.roots;
        setupMode = !state.initialized;

        if (setupMode) {
          await resizeSetupWindow();
          await windowHandle.center();
          await windowHandle.setFocus();
        } else {
          await resizeSearchWindow(0);
        }

        unlistenFocus = await windowHandle.onFocusChanged(async ({ payload }) => {
          if (!payload) {
            if (!setupMode && viewMode === "search" && !folderPickerOpen && settings.hideOnBlur) {
              await windowHandle.hide();
            }
            return;
          }

          if (!setupMode) {
            if (viewMode === "config") {
              await resizeConfigWindow();
            }
            await tick();
            input?.focus();
            input?.select();
          }
        });
      } catch (error) {
        setupMode = true;
        setupError = "LookPlox could not load its setup state: " + String(error);
        await resizeSetupWindow();
        await windowHandle.center();
        await windowHandle.setFocus();
      }
    };

    void initialize();

    return () => {
      unlistenFocus?.();
      if (indexPoll !== undefined) {
        window.clearInterval(indexPoll);
      }
    };
  });
</script>

<svelte:window onkeydown={handleKeydown} />

{#if setupMode}
  <main class="setup-shell">
    <section class="setup-card" aria-label="LookPlox initial setup">
      <header class="setup-header">
        <div>
          <div class="setup-kicker">LOOKPLOX</div>
          <h1>Choose what LookPlox should index</h1>
          <p>
            Select one or more folders. LookPlox will search file names inside
            them and keep the index updated.
          </p>
        </div>
      </header>

      <div class="roots-panel">
        <div class="roots-heading">
          <span>Folders to index</span>
          <span class="root-count">{roots.length}</span>
        </div>

        {#if roots.length > 0}
          <div class="roots-list">
            {#each roots as root, index}
              <div class="root-row">
                <span class="folder-mark">□</span>
                <span class="root-path">{root}</span>
                <button
                  class="remove-root"
                  type="button"
                  onclick={() => removeRoot(index)}
                  disabled={indexing}
                  aria-label={"Remove " + root}
                >×</button>
              </div>
            {/each}
          </div>
        {:else}
          <div class="empty-roots">
            No folders selected yet.
          </div>
        {/if}

        <button class="add-folder" type="button" onclick={addFolder} disabled={indexing}>
          <span>＋</span>
          <span>Add folder</span>
        </button>
      </div>

      {#if indexing}
        <div class="indexing-status">
          <div class="progress-track">
            <div class="progress-indicator"></div>
          </div>
          <div class="status-text">
            <span>{cancelRequested ? "Canceling…" : "Building search index…"}</span>
            <span>{indexedCount.toLocaleString()} files scanned</span>
          </div>
          <button
            class="cancel-indexing"
            type="button"
            onclick={cancelIndexing}
            disabled={cancelRequested}
          >
            {cancelRequested ? "Canceling…" : "Cancel"}
          </button>
        </div>
      {:else}
        <div class="setup-footer">
          <span class:error={Boolean(setupError)}>
            {setupError || "Ready to build the local search index."}
          </span>
          <button
            class="start-indexing"
            type="button"
            onclick={finishSetup}
            disabled={roots.length === 0}
          >
            Start indexing
          </button>
        </div>
      {/if}
    </section>
  </main>
{:else if viewMode === "config"}
  <main class="config-shell">
    <section class="settings-card" aria-label="LookPlox settings">
      <header class="settings-header">
        <button class="settings-back" type="button" onclick={backToSearch} aria-label="Back to search">‹</button>
        <div>
          <div class="setup-kicker">LOOKPLOX</div>
          <h1>Settings</h1>
          <p>Control search results, appearance, window behavior, and indexing.</p>
        </div>
      </header>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Search</span>
        </div>

        <div class="settings-options">
          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Result limit</span>
              <span class="settings-description">Maximum number of matching files returned by each search.</span>
            </span>
            <select
              value={settings.resultLimit}
              onchange={(event) =>
                updateSettings({ resultLimit: Number((event.currentTarget as HTMLSelectElement).value) })}
            >
              <option value="6">6</option>
              <option value="12">12</option>
              <option value="24">24</option>
              <option value="50">50</option>
            </select>
          </label>

          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Show file paths</span>
              <span class="settings-description">Display the full path below each search result.</span>
            </span>
            <input
              class="settings-switch"
              type="checkbox"
              checked={settings.showPaths}
              onchange={(event) =>
                updateSettings({ showPaths: (event.currentTarget as HTMLInputElement).checked })}
            />
          </label>
        </div>
      </div>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Appearance</span>
        </div>

        <div class="settings-options">
          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Theme</span>
              <span class="settings-description">Choose the interface appearance.</span>
            </span>
            <select
              value={settings.theme}
              onchange={(event) =>
                updateSettings({
                  theme: (event.currentTarget as HTMLSelectElement).value as Theme,
                })}
            >
              <option value="system">System</option>
              <option value="light">Light</option>
              <option value="dark">Dark</option>
            </select>
          </label>

          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Preview image files</span>
              <span class="settings-description">Show image thumbnails instead of the generic file icon for supported image files.</span>
            </span>
            <input
              class="settings-switch"
              type="checkbox"
              checked={settings.previewImages}
              onchange={(event) =>
                updateSettings({ previewImages: (event.currentTarget as HTMLInputElement).checked })}
            />
          </label>

          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Preview application icons</span>
              <span class="settings-description">Show the native application icon for .app bundles on macOS.</span>
            </span>
            <input
              class="settings-switch"
              type="checkbox"
              checked={settings.previewApplications}
              onchange={(event) =>
                updateSettings({ previewApplications: (event.currentTarget as HTMLInputElement).checked })}
            />
          </label>
        </div>
      </div>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Window</span>
        </div>

        <div class="settings-options">
          <label class="settings-row">
            <span class="settings-copy">
              <span class="settings-title">Hide when focus is lost</span>
              <span class="settings-description">Automatically hide the search window after clicking another app.</span>
            </span>
            <input
              class="settings-switch"
              type="checkbox"
              checked={settings.hideOnBlur}
              onchange={(event) =>
                updateSettings({ hideOnBlur: (event.currentTarget as HTMLInputElement).checked })}
            />
          </label>
        </div>
      </div>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Storage</span>
        </div>

        {#if storageLocations}
          <div class="settings-storage-list">
            <div class="settings-storage-row">
              <div class="settings-copy">
                <span class="settings-title">Settings database</span>
                <span class="settings-description">Folder containing LookPlox's settings.sqlite3.</span>
                <span class="settings-path">{storageLocations.settingsDbDir}</span>
                <span class="settings-file-path">{storageLocations.settingsDbPath}</span>
              </div>
              <button
                class="secondary-action"
                type="button"
                onclick={() => chooseStorageLocation("settings-db")}
                disabled={storageChanging || indexing}
              >
                Change
              </button>
            </div>

            <div class="settings-storage-row">
              <div class="settings-copy">
                <span class="settings-title">Search index</span>
                <span class="settings-description">Folder containing the local Tantivy search index.</span>
                <span class="settings-path">{storageLocations.indexPath}</span>
              </div>
              <button
                class="secondary-action"
                type="button"
                onclick={() => chooseStorageLocation("index")}
                disabled={storageChanging || indexing}
              >
                Change
              </button>
            </div>
          </div>

          <div class:error={Boolean(storageError)} class="settings-storage-note">
            {#if storageError}
              {storageError}
            {:else}
              Changing either location moves the existing data and restarts LookPlox.
            {/if}
          </div>
        {:else}
          <div class:error={Boolean(storageError)} class="settings-storage-note">
            {storageError || "Loading storage locations…"}
          </div>
        {/if}
      </div>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Index</span>
        </div>

        <div class="settings-action-row">
          <div class="settings-copy">
            <span class="settings-title">Rebuild search index</span>
            <span class="settings-description">Rescan all tracked folders and replace the current local index.</span>
          </div>
          <button class="secondary-action" type="button" onclick={reindexNow} disabled={indexing}>
            {indexing ? "Rebuilding…" : "Rebuild"}
          </button>
        </div>

        <div class="settings-roots compact-roots">
          <div class="settings-section-heading subheading">
            <span>Tracked folders</span>
            <span class="root-count">{roots.length}</span>
          </div>

          {#each roots as root}
            <div class="settings-root-row">
              <span class="folder-mark" aria-hidden="true">
                <svg viewBox="0 0 24 24" fill="none">
                  <path d="M3.5 7.5h6l2 2h9v8.75a1.25 1.25 0 0 1-1.25 1.25H4.75A1.25 1.25 0 0 1 3.5 18.25V7.5Z" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                  <path d="M3.5 7.5V6.25A1.25 1.25 0 0 1 4.75 5h4l2 2h8.5A1.25 1.25 0 0 1 20.5 8.25V9.5" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                </svg>
              </span>
              <span class="root-path">{root}</span>
              <button
                class="remove-root"
                type="button"
                onclick={() => removeTrackedFolder(root)}
                disabled={indexing || roots.length <= 1}
                aria-label={"Stop tracking " + root}
              >×</button>
            </div>
          {/each}
        </div>

        <button class="add-folder settings-add" type="button" onclick={addIndexFolder} disabled={indexing}>
          <span>＋</span>
          <span>Add folder</span>
        </button>
      </div>

      <div class="settings-section settings-about">
        <div class="settings-section-heading">
          <span>About</span>
        </div>
        <div class="about-row">
          <span>Version</span>
          <strong>0.1.0</strong>
        </div>
        <div class="about-row">
          <span>Search engine</span>
          <strong>Local index</strong>
        </div>
      </div>

      <div class="settings-footer">
        <div class:error={Boolean(setupError)}>
          {#if setupError}
            {setupError}
          {:else if indexing}
            {cancelRequested ? "Canceling…" : "Rebuilding search index…"} · {indexedCount.toLocaleString()} scanned
          {:else if indexMessage}
            {indexMessage}
          {:else}
            <span>{storageChanging ? "Restarting LookPlox…" : "Settings are saved automatically."}</span>
          {/if}
        </div>
        {#if indexing}
          <button class="cancel-indexing" type="button" onclick={cancelIndexing} disabled={cancelRequested}>
            {cancelRequested ? "Canceling…" : "Cancel"}
          </button>
        {/if}
      </div>
    </section>
  </main>
{:else if viewMode === "help"}
  <main class="config-shell">
    <section class="settings-card help-card" aria-label="LookPlox commands">
      <header class="settings-header">
        <button class="settings-back" type="button" onclick={backToSearch} aria-label="Back to search">‹</button>
        <div>
          <div class="setup-kicker">LOOKPLOX</div>
          <h1>Commands</h1>
          <p>Type a slash command in the search field and press Enter. Prefix an application search with @.</p>
        </div>
      </header>

      <div class="command-help-list">
        <div class="command-help-row application-help-row">
          <span class="command-name">@</span>
          <span class="command-description">Search applications only, for example @Safari</span>
        </div>
        {#each COMMANDS as item}
          <button class="command-help-row" type="button" onclick={() => executeCommand(item.command)}>
            <span class="command-name">{item.command}</span>
            <span class="command-description">{item.description}</span>
          </button>
        {/each}
      </div>
    </section>
  </main>
{:else}
  <main class="shell">
    <div class="search-bar">
      <span class="prompt">›</span>
      <input
        bind:this={input}
        bind:value={query}
        oninput={() => search()}
        placeholder="Search files · @ apps · / commands"
        autocomplete="off"
        spellcheck="false"
        aria-label="Search files, applications, or commands"
      />
      {#if searching}
        <span class="status">Searching</span>
      {:else if indexMessage}
        <span class="status">{indexMessage}</span>
      {:else if isApplicationQuery(query)}
        <span class="status application-status">Applications</span>
      {:else if results.length > 0}
        <span class="status">{results.length}</span>
      {/if}
      <button
        class="add-index-folder"
        type="button"
        onclick={addIndexFolder}
        disabled={indexing}
        title="Add folder to index"
        aria-label="Add folder to index"
      >
        +
      </button>
    </div>

    {#if commandMatches.length > 0}
      <section class="results commands" aria-label="Commands">
        {#each commandMatches as item, index}
          <button
            class:selected={index === selected}
            class="result command-row"
            type="button"
            onclick={() => executeCommand(item.command)}
          >
            <span class="command-icon" aria-hidden="true">/</span>
            <span class="result-text">
              <span class="name command-name">{item.command}</span>
              <span class="path command-description">{item.description}</span>
            </span>
          </button>
        {/each}
      </section>
    {:else if results.length > 0}
      <section class="results" aria-label="Search results">
        {#each results as result, index}
          <button
            class:selected={index === selected}
            class="result"
            type="button"
            onclick={() => openResult(result)}
          >
            <span class:folder-icon={result.is_dir} class:file-icon={!result.is_dir} class="icon" aria-hidden="true">
              {#if result.preview}
                <img class="result-preview" src={result.preview} alt="" />
              {:else if result.is_dir}
                <svg viewBox="0 0 24 24" fill="none">
                  <path d="M3.5 7.5h6l2 2h9v8.75a1.25 1.25 0 0 1-1.25 1.25H4.75A1.25 1.25 0 0 1 3.5 18.25V7.5Z" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                  <path d="M3.5 7.5V6.25A1.25 1.25 0 0 1 4.75 5h4l2 2h8.5A1.25 1.25 0 0 1 20.5 8.25V9.5" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                </svg>
              {:else}
                <svg viewBox="0 0 24 24" fill="none">
                  <path d="M7 3.75h7.2L18.5 8v11.25H7A1.25 1.25 0 0 1 5.75 18V5A1.25 1.25 0 0 1 7 3.75Z" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                  <path d="M14 3.75V8h4.25" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
                </svg>
              {/if}
            </span>
            <span class="result-text">
              <span class="name">{result.name}</span>
              {#if settings.showPaths}
                <span class="path">{result.path}</span>
              {/if}
            </span>
          </button>
        {/each}
      </section>
    {:else if searchSuggestion}
      <section class="suggestion" aria-label="Search suggestion">
        <button type="button" class="suggestion-row" onclick={useSearchSuggestion}>
          <span class="suggestion-label">もしかして</span>
          <span class="suggestion-name">{searchSuggestion}</span>
        </button>
      </section>
    {/if}
  </main>
{/if}
