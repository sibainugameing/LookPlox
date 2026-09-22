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

  type CommandItem = {
    command: string;
    description: string;
  };

  const COMMANDS: CommandItem[] = [
    { command: "/config", description: "Open LookPlox settings" },
    { command: "/add-folder", description: "Add a folder to the search index" },
    { command: "/help", description: "Show available commands" },
  ];

  let query = "";
  let results: SearchResult[] = [];
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

  const windowHandle = getCurrentWindow();

  async function resizeSearchWindow(resultCount = 0) {
    const visibleResults = Math.min(Math.max(resultCount, 1), 8);
    const height =
      resultCount === 0
        ? 104
        : 12 + 68 + 10 + 16 + visibleResults * 56 + 10;

    await windowHandle.setSize(new LogicalSize(760, height));
  }

  async function resizeSetupWindow() {
    await windowHandle.setSize(new LogicalSize(760, 480));
  }

  async function resizeConfigWindow() {
    await windowHandle.setSize(new LogicalSize(760, 520));
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

  async function finishSetup() {
    setupError = "";

    if (roots.length === 0) {
      setupError = "Add at least one folder to continue.";
      return;
    }

    indexing = true;
    cancelRequested = false;
    indexedCount = 0;

    try {
      await invoke("start_indexing", { roots });

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
              setupError = "";
              return;
            }

            if (status.error) {
              setupError = status.error;
              return;
            }

            setupMode = false;
            await resizeSearchWindow();
            await windowHandle.center();
            await windowHandle.setFocus();
            await tick();
            input?.focus();
            input?.select();
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
      setupError = String(error);
    }
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

      indexing = true;
      cancelRequested = false;
      indexedCount = 0;
      indexMessage = "Indexing folder…";

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
    } catch (error) {
      setupError = String(error);
      indexMessage = "";
    } finally {
      folderPickerOpen = false;
    }
  }

  async function showConfig() {
    viewMode = "config";
    query = "";
    results = [];
    commandMatches = [];
    selected = 0;
    await resizeConfigWindow();
    await windowHandle.center();
    await windowHandle.setFocus();
  }

  async function showHelp() {
    viewMode = "help";
    query = "";
    results = [];
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
    commandMatches = [];
    selected = 0;
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
    commandMatches = [];
    selected = 0;
    await resizeSearchWindow(0);
    await windowHandle.hide();
  }

  function normalizedCommandQuery(value: string) {
    return value.trim().replace(/^／/, "/");
  }

  function isCommandQuery(value: string) {
    return normalizedCommandQuery(value).startsWith("/");
  }

  async function search() {
    const currentRequest = ++requestId;
    if (indexMessage) {
      indexMessage = "";
    }
    const rawValue = query.trim();
    const value = normalizedCommandQuery(rawValue);

    if (value.startsWith("/")) {
      commandMatches = COMMANDS.filter((item) =>
        item.command.startsWith(value.toLowerCase()),
      );
      results = [];
      selected = Math.min(selected, Math.max(commandMatches.length - 1, 0));
      await resizeSearchWindow(commandMatches.length);
      return;
    }

    commandMatches = [];

    if (!value) {
      results = [];
      selected = 0;
      await resizeSearchWindow(0);
      return;
    }

    searching = true;

    try {
      const nextResults = await invoke<SearchResult[]>("search_files", {
        query: value,
        limit: 12,
      });

      if (currentRequest === requestId) {
        results = nextResults;
        selected = Math.min(selected, Math.max(nextResults.length - 1, 0));
        await resizeSearchWindow(nextResults.length);
      }
    } catch (error) {
      console.error("LookPlox search failed:", error);
      if (currentRequest === requestId) {
        results = [];
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
            if (!setupMode && viewMode === "search" && !folderPickerOpen) {
              await windowHandle.hide();
            }
            return;
          }

          if (!setupMode) {
            await resizeSearchWindow(results.length);
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
          <p>Manage the folders LookPlox tracks for continuous file-name search.</p>
        </div>
      </header>

      <div class="settings-section">
        <div class="settings-section-heading">
          <span>Tracked folders</span>
          <span class="root-count">{roots.length}</span>
        </div>

        <div class="settings-roots">
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

      <div class="settings-footer">
        <div class:error={Boolean(setupError)}>
          {#if setupError}
            {setupError}
          {:else if indexing}
            {cancelRequested ? "Canceling…" : "Indexing folder…"}
          {:else if indexMessage}
            {indexMessage}
          {:else}
            <span>Folders are monitored continuously while LookPlox is running.</span>
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
          <p>Type a slash command in the search field and press Enter.</p>
        </div>
      </header>

      <div class="command-help-list">
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
        placeholder="Search files or /commands"
        autocomplete="off"
        spellcheck="false"
        aria-label="Search files or commands"
      />
      {#if searching}
        <span class="status">Searching</span>
      {:else if indexMessage}
        <span class="status">{indexMessage}</span>
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
              {#if result.is_dir}
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
              <span class="path">{result.path}</span>
            </span>
          </button>
        {/each}
      </section>
    {/if}
  </main>
{/if}