<script lang="ts">
  import { onMount, tick } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";

  type SearchResult = {
    name: string;
    path: string;
  };

  let query = "";
  let results: SearchResult[] = [];
  let selected = 0;
  let input: HTMLInputElement;
  let searching = false;
  let requestId = 0;

  const windowHandle = getCurrentWindow();

  async function showSearchWindow() {
    await windowHandle.show();
    await windowHandle.center();
    await windowHandle.setFocus();
    await tick();
    input?.focus();
    input?.select();
  }

  async function hideSearchWindow() {
    query = "";
    results = [];
    selected = 0;
    await windowHandle.hide();
  }

  async function search() {
    const currentRequest = ++requestId;
    const value = query.trim();

    if (!value) {
      results = [];
      selected = 0;
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

  onMount(async () => {
    const unlistenFocus = await windowHandle.onFocusChanged(async ({ payload }) => {
      if (!payload) {
        await windowHandle.hide();
        return;
      }

      await tick();
      input?.focus();
      input?.select();
    });

    return () => {
      unlistenFocus();
    };
  });
</script>

<svelte:window onkeydown={handleKeydown} />

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
