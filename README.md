# LookPlox

Fast local file search and indexing application.

## Current target

- macOS first
- Windows-compatible architecture
- Local-only indexing
- Minimal floating search UI

## Stack

- Tauri 2
- Rust
- Tantivy
- notify
- Svelte
- TypeScript

## MVP

1. Index common user folders.
2. Keep the index updated with filesystem events.
3. Search file names from the local index.
4. Open the selected file.
5. Show the search window with a global shortcut.

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

The initial macOS shortcut is:

~~~
Command + Shift + Space
~~~

Command + Space is intentionally not used in the MVP because macOS commonly reserves it for Spotlight.

## Index location

The search index is stored in the application's platform-specific application-data directory.

No file contents are uploaded. The MVP indexes file metadata and names locally.
