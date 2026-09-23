# LookPlox

Fast local file search and indexing application.

## Current target

- macOS first
- Windows-compatible architecture
- Local-only indexing
- Minimal floating search UI
- First-run folder selection

## Stack

- Tauri 2
- Rust
- Tantivy
- notify
- Svelte
- TypeScript

## MVP

1. Let the user choose folders to index on first launch.
2. Build a local filename index from the selected folders.
3. Keep the index updated with filesystem events.
4. Search file names from the local index.
5. Preview supported image files and native macOS application icons in search results.
5. Open the selected file.
6. Show the search window with a global shortcut.

## Development

Requirements:

- Node.js
- Rust toolchain
- macOS development environment for the first target

Install dependencies:

~~~
npm install
~~~

Run:

~~~
npm run tauri dev
~~~

The macOS shortcut is:

~~~
Option + Space
~~~

Command + Space is intentionally not used because macOS commonly reserves it for Spotlight.

## First launch

LookPlox starts with a setup screen where you choose the folders that should be indexed.

Folders are stored in the application's local application-data directory as configuration. The search index is stored there as well.

No file contents are uploaded. The MVP indexes file metadata and names locally.
