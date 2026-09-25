<script lang="ts">
  import { onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { LogicalSize, PhysicalPosition } from "@tauri-apps/api/dpi";
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

  type SuggestedFolder = {
    id: string;
    name: string;
    path: string;
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
  let setupStep = 1;
  let roots: string[] = [];
  let suggestedFolders: SuggestedFolder[] = [];
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
  const WINDOW_CENTER_Y_OFFSET_PX = -56;

  async function centerWindowSlightlyAbove() {
    await windowHandle.center();
    const position = await windowHandle.outerPosition();
    await windowHandle.setPosition(
      new PhysicalPosition(position.x, position.y + WINDOW_CENTER_Y_OFFSET_PX),
    );
  }

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
    await centerWindowSlightlyAbove();
  }

  async function resizeSetupWindow() {
    await windowHandle.setSize(new LogicalSize(760, 580));
    await centerWindowSlightlyAbove();
  }

  async function resizeConfigWindow() {
    await windowHandle.setSize(new LogicalSize(760, 600));
    await centerWindowSlightlyAbove();
  }

  async function resizeHelpWindow() {
    await windowHandle.setSize(new LogicalSize(760, 360));
    await centerWindowSlightlyAbove();
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

  function isRootCovered(path: string) {
    return roots.some((root) => isSameOrChildPath(path, root));
  }

  function addSuggestedFolder(path: string) {
    addRootCandidate(path);
  }

  async function loadSuggestedFolders() {
    try {
      suggestedFolders = await invoke<SuggestedFolder[]>("get_suggested_folders");
    } catch (error) {
      console.error("LookPlox suggested folders could not be loaded:", error);
      suggestedFolders = [];
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

  function goToSetupStep(step: number) {
    if (indexing) {
      return;
    }

    setupError = "";

    if (step > setupStep) {
      return;
    }

    if (step === 5 && roots.length === 0) {
      return;
    }

    setupStep = Math.max(1, Math.min(5, step));
    void resizeSetupWindow();
  }

  function continueSetup() {
    setupError = "";

    if (setupStep === 1) {
      setupStep = 2;
      void resizeSetupWindow();
      return;
    }

    if (setupStep === 2) {
      if (roots.length === 0) {
        setupError = "Choose at least one folder to continue.";
        return;
      }

      setupStep = 3;
      void resizeSetupWindow();
      return;
    }

    if (setupStep === 3) {
      setupStep = 4;
      void resizeSetupWindow();
      return;
    }

    if (setupStep === 4) {
      setupStep = 5;
      void resizeSetupWindow();
    }
  }

  async function finishSetup() {
    setupError = "";

    if (roots.length === 0) {
      setupStep = 2;
      setupError = "Choose at least one folder to continue.";
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

  function isCommandQuery(value: string) {
    return normalizedCommandQuery(value).startsWith("/");
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
      return;
    }

    if (event.key === "Enter" && searchSuggestion) {
      event.preventDefault();
      await useSearchSuggestion();
    }
  }

  onMount(() => {
    let unlistenFocus: (() => void) | undefined;

    const initialize = async () => {
      await Promise.all([
        loadSettings(),
        loadStorageLocations(),
        loadSuggestedFolders(),
      ]);

      try {
        const state = await invoke<SetupState>("get_setup_state");
        roots = state.roots;
        setupMode = !state.initialized;

        if (setupMode) {
          await resizeSetupWindow();
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
  {#if indexing}
    <main class="indexing-shell">
      <section class="setup-card indexing-card" aria-label="LookPlox indexing">
        <div class="indexing-kicker">LOOKPLOX</div>

        <div class="indexing-icon" aria-hidden="true">
          <span class="indexing-ring"></span>
          <span class="indexing-glyph">⌕</span>
        </div>

        <h1>Building your search index</h1>
        <p class="indexing-lead">
          LookPlox is scanning the folders you selected and building a local search index.
        </p>

        <div class="indexing-summary">
          <div>
            <span>Folders</span>
            <strong>{roots.length}</strong>
          </div>
          <div>
            <span>Items scanned</span>
            <strong>{indexedCount.toLocaleString()}</strong>
          </div>
        </div>

        <div class="indexing-progress-panel">
          <div class="progress-track">
            <div class="progress-indicator"></div>
          </div>
          <div class="status-text indexing-status-text">
            <span>{cancelRequested ? "Canceling…" : "Building local index…"}</span>
            <span>{indexedCount.toLocaleString()} items scanned</span>
          </div>
        </div>

        <div class="indexing-note">
          <strong>This may take a while on a large folder.</strong>
          <span>You can cancel the scan at any time. Your existing search window will appear when the initial index is ready.</span>
        </div>

        <button
          class="cancel-indexing indexing-cancel"
          type="button"
          onclick={cancelIndexing}
          disabled={cancelRequested}
        >
          {cancelRequested ? "Canceling…" : "Cancel indexing"}
        </button>
      </section>
    </main>
  {:else}
    <main class="setup-shell">
      <section class="setup-card setup-wizard" aria-label="LookPlox initial setup">
        <header class="wizard-header">
          <div class="setup-kicker">LOOKPLOX</div>
          <div class="wizard-progress" aria-label={"Setup step " + setupStep + " of 5"}>
            {#each [1, 2, 3, 4, 5] as step}
              <button
                class:active={setupStep === step}
                class:done={setupStep > step}
                class="wizard-step"
                type="button"
                onclick={() => goToSetupStep(step)}
                disabled={step > setupStep || (step === 5 && roots.length === 0)}
                aria-label={"Go to step " + step}
              >
                <span>{step}</span>
              </button>
              {#if step < 5}<span class="wizard-line"></span>{/if}
            {/each}
          </div>
        </header>

        {#if setupStep === 1}
          <div class="wizard-page wizard-welcome">
            <div class="wizard-icon" aria-hidden="true">
              <span>⌕</span>
            </div>
            <h1>Welcome to LookPlox</h1>
            <p class="wizard-lead">
              Set up your local file search before the first index is built.
            </p>

            <div class="wizard-points">
              <div>
                <strong>Private by default</strong>
                <span>The search index stays on this computer.</span>
              </div>
              <div>
                <strong>Choose what gets indexed</strong>
                <span>LookPlox only searches folders you add.</span>
              </div>
              <div>
                <strong>Fast after setup</strong>
                <span>Search uses a local index instead of rescanning every time.</span>
              </div>
            </div>

            <div class="wizard-hotkey">
              <span>Search window shortcut</span>
              <kbd>Alt</kbd><span>+</span><kbd>Space</kbd>
            </div>
          </div>

          <footer class="wizard-footer">
            <span>Step 1 of 5</span>
            <button class="wizard-primary" type="button" onclick={continueSetup}>
              Continue
            </button>
          </footer>
        {:else if setupStep === 2}
          <div class="wizard-page">
            <div class="wizard-page-heading">
              <div>
                <div class="wizard-step-label">STEP 2</div>
                <h1>Choose folders</h1>
                <p>Select the locations LookPlox should search.</p>
              </div>
              <span class="root-count">{roots.length}</span>
            </div>

            {#if roots.length > 0}
              <div class="roots-list wizard-roots-list">
                {#each roots as root, index}
                  <div class="root-row">
                    <span class="folder-mark" aria-hidden="true">⌑</span>
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
              <div class="wizard-empty">
                <strong>No folders selected</strong>
                <span>Choose a common folder below or add another folder.</span>
              </div>
            {/if}

            {#if suggestedFolders.length > 0}
              <div class="quick-folders wizard-quick-folders">
                <div class="quick-folders-heading">
                  <span>Common folders</span>
                  <span>Quick add</span>
                </div>
                <div class="quick-folders-grid">
                  {#each suggestedFolders as folder}
                    {@const covered = isRootCovered(folder.path)}
                    <button
                      class:covered={covered}
                      class="quick-folder"
                      type="button"
                      onclick={() => addSuggestedFolder(folder.path)}
                      disabled={indexing || covered}
                      title={folder.path}
                    >
                      <span class="quick-folder-icon" aria-hidden="true">⌑</span>
                      <span>{folder.name}</span>
                      <span class="quick-folder-state">{covered ? "Added" : "Add"}</span>
                    </button>
                  {/each}
                </div>
              </div>
            {/if}

            <button class="add-folder wizard-add-folder" type="button" onclick={addFolder} disabled={indexing}>
              <span>＋</span>
              <span>Choose another folder</span>
            </button>
          </div>

          <footer class="wizard-footer">
            <button class="wizard-secondary" type="button" onclick={() => goToSetupStep(1)} disabled={indexing}>
              Back
            </button>
            <span class:error={Boolean(setupError)}>{setupError || "You can add more folders later."}</span>
            <button class="wizard-primary" type="button" onclick={continueSetup} disabled={roots.length === 0 || indexing}>
              Continue
            </button>
          </footer>
        {:else if setupStep === 3}
          <div class="wizard-page wizard-settings-page">
            <div class="wizard-page-heading">
              <div>
                <div class="wizard-step-label">STEP 3</div>
                <h1>Search preferences</h1>
                <p>Choose how search results should behave.</p>
              </div>
            </div>

            <div class="wizard-setting-list">
              <label class="settings-row">
                <span class="settings-copy">
                  <span class="settings-title">Result limit</span>
                  <span class="settings-description">Maximum number of matching items returned by each search.</span>
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

            <div class="wizard-info-card">
              <strong>These settings can be changed later.</strong>
              <span>Your choices are saved automatically while you go through setup.</span>
            </div>
          </div>

          <footer class="wizard-footer">
            <button class="wizard-secondary" type="button" onclick={() => goToSetupStep(2)}>
              Back
            </button>
            <span>Step 3 of 5</span>
            <button class="wizard-primary" type="button" onclick={continueSetup}>
              Continue
            </button>
          </footer>
        {:else if setupStep === 4}
          <div class="wizard-page wizard-settings-page">
            <div class="wizard-page-heading">
              <div>
                <div class="wizard-step-label">STEP 4</div>
                <h1>Appearance</h1>
                <p>Set the visual style and result previews.</p>
              </div>
            </div>

            <div class="wizard-setting-list">
              <label class="settings-row">
                <span class="settings-copy">
                  <span class="settings-title">Theme</span>
                  <span class="settings-description">Choose whether LookPlox follows the system appearance.</span>
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
                  <span class="settings-description">Show thumbnails for supported image files in search results.</span>
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
                  <span class="settings-description">Show native application icons for supported app bundles.</span>
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

            <div class="wizard-info-card">
              <strong>Preview generation can use extra system work.</strong>
              <span>You can turn previews off later if you prefer a leaner search window.</span>
            </div>
          </div>

          <footer class="wizard-footer">
            <button class="wizard-secondary" type="button" onclick={() => goToSetupStep(3)}>
              Back
            </button>
            <span>Step 4 of 5</span>
            <button class="wizard-primary" type="button" onclick={continueSetup}>
              Continue
            </button>
          </footer>
        {:else}
          <div class="wizard-page wizard-review">
            <div class="wizard-page-heading">
              <div>
                <div class="wizard-step-label">STEP 5</div>
                <h1>Ready to build</h1>
                <p>Review your setup before the first index is created.</p>
              </div>
            </div>

            <div class="review-card">
              <div class="review-card-heading">
                <span>Folders to index</span>
                <span>{roots.length}</span>
              </div>
              <div class="review-list">
                {#each roots as root}
                  <div class="review-row">
                    <span class="folder-mark" aria-hidden="true">⌑</span>
                    <span class="root-path">{root}</span>
                  </div>
                {/each}
              </div>
            </div>

            <div class="review-grid">
              <div class="review-stat">
                <span>Results</span>
                <strong>{settings.resultLimit}</strong>
                <small>max items</small>
              </div>
              <div class="review-stat">
                <span>Paths</span>
                <strong>{settings.showPaths ? "On" : "Off"}</strong>
                <small>result paths</small>
              </div>
              <div class="review-stat">
                <span>Theme</span>
                <strong>{settings.theme === "system" ? "System" : settings.theme === "light" ? "Light" : "Dark"}</strong>
                <small>appearance</small>
              </div>
              <div class="review-stat">
                <span>Previews</span>
                <strong>{settings.previewImages || settings.previewApplications ? "On" : "Off"}</strong>
                <small>image / app icons</small>
              </div>
            </div>

            <div class="review-note">
              <strong>What happens next</strong>
              <span>The selected folders will be scanned and a local index will be created. The search window opens automatically when it is ready.</span>
            </div>
          </div>

          <footer class="wizard-footer">
            <button class="wizard-secondary" type="button" onclick={() => goToSetupStep(4)}>
              Back
            </button>
            <span class:error={Boolean(setupError)}>{setupError || "Ready to build the local search index."}</span>
            <button class="wizard-primary" type="button" onclick={finishSetup} disabled={roots.length === 0}>
              Build index
            </button>
          </footer>
        {/if}
      </section>
    </main>
  {/if}
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
