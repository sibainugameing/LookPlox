<script lang="ts">
  import { onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { LogicalSize } from "@tauri-apps/api/dpi";
  import { open } from "@tauri-apps/plugin-dialog";

  type SearchResult = {
    name: string;
    path: string;
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

  let query = "";
  let results: SearchResult[] = [];
  let selected = 0;
  let input: HTMLInputElement;
  let searching = false;
  let requestId = 0;

  let setupMode = true;
  let roots: string[] = [];
  let indexing = false;
  let cancelRequested = false;
  let indexedCount = 0;
  let setupError = "";
  let indexPoll: number | undefined;

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

  async function hideSearchWindow() {
    if (setupMode) {
      return;
    }

    query = "";
    results = [];
    selected = 0;
    await resizeSearchWindow(0);
    await windowHandle.hide();
  }

  async function search() {
    const currentRequest = ++requestId;
    const value = query.trim();

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
      if (results.length > 0) {
        selected = Math.min(selected + 1, results.length - 1);
      }
      return;
    }

    if (event.key === "ArrowUp") {
      event.preventDefault();
      if (results.length > 0) {
        selected = Math.max(selected - 1, 0);
      }
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
            if (!setupMode) {
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
{:else}
  <main class="shell">
    <div class="search-bar">
      <span class="prompt">›</span>
      <input
        bind:this={input}
        bind:value={query}
        oninput={() => search()}
        placeholder="Search files"
        autocomplete="off"
        spellcheck="false"
        aria-label="Search files"
      />
      {#if searching}
        <span class="status">Searching</span>
      {:else if results.length > 0}
        <span class="status">{results.length}</span>
      {/if}
    </div>

    {#if results.length > 0}
      <section class="results" aria-label="Search results">
        {#each results as result, index}
          <button
            class:selected={index === selected}
            class="result"
            type="button"
            onclick={() => openResult(result)}
          >
            <span class="icon">□</span>
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
