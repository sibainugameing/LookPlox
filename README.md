# LookPlox

Fast local file search and indexing application.

## Platform support

- macOS
- Windows
- Linux
- Local-only indexing
- Minimal floating search UI
- First-run folder selection

OS-specific application discovery, application launching, application-container handling, and native window effects are isolated under `src-tauri/src/platform/`.

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
5. Search installed applications with the `@` prefix, independently of tracked file folders.
6. Show typo suggestions with “もしかして” when a search has a close match.
7. Preview supported image files and native application icons where the platform implementation supports them.
8. Open the selected file.
9. Show the search window with a global shortcut.

## Development

Requirements:

- Node.js
- Rust toolchain
- A development environment for the target OS
- Tauri's OS-specific system dependencies

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

## Search syntax

- Normal search: `Safari`
- Application search: `@Safari` (full-width `＠` is also accepted)
- Commands: `/config`, `/add-folder`, `/help`
- Search suggestions appear when a close filename or application match is found.

Application search uses OS application locations rather than requiring the user to add `/Applications` to the file index. On macOS, Spotlight is used when available with a filesystem fallback, and the application catalog is cached for 30 seconds to keep typing responsive.

Indexing can be canceled from the first-run setup screen or from Settings.

## Cross-platform build check

GitHub Actions runs the frontend build, Rust tests, and a Tauri build without bundling on macOS, Windows, and Linux.

Platform-specific Tauri configuration is kept in files such as `src-tauri/tauri.macos.conf.json`, while the base `tauri.conf.json` remains platform-neutral.
