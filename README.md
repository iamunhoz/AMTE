# AMTE

AMTE is a local-first Markdown project editor for focused writing. Its desktop editor is a Tauri application with a CodeMirror 6 source buffer, a rendered Markdown preview, live outline and word analysis, and offline bundled themes.

## Project workflow

- `Ctrl+O` opens a native **folder** picker. AMTE keeps one opened project; the title bar presents its canonical path (or `No project open`).
- The project tree is lazy: opening a project reads its root, and expanding a directory reads that directory once. Directories appear before `.md` and `.markdown` files in stable case-insensitive order. Hidden entries, `.git`, `node_modules`, `target`, `dist`, and symlinks are omitted.
- Select a Markdown file to use it in the single CodeMirror buffer. If the buffer is dirty, changing the file or project prompts for **Save**, **Discard**, or **Cancel**. Cancel leaves the project, selection, document, dirty state, and autosave unchanged.
- `Ctrl+S` saves the active file. With an open project but no selected file, it opens Save As and accepts a new `.md` file only in an existing directory inside that project. Untitled text can be edited without a project but cannot be saved until one is opened.
- Project state is stored atomically as `last-project-v1`, with a canonical root and optional relative file. A stale file restores its project without opening a document; a stale root is discarded. A valid legacy `last-document-path` is migrated once.

## Keyboard navigation and editing

- `Ctrl+O`: open project folder; `Ctrl+S`: save; `Ctrl+P`: source/preview; `Ctrl+T`: toggle theme.
- `Ctrl++` / `Ctrl+-`: application zoom; `Ctrl+[` / `Ctrl+]`: page width; `Ctrl+Backspace`: reset page width.
- Plain `F1` focuses the selected (or first) project-tree item, `F2` focuses CodeMirror without changing its caret or selection, and `F3` focuses the current (or first) outline chapter. Tree rows support arrows, Enter, and Space for navigation and expansion.
- Plain `F5` opens Find in document. Type a literal, case-insensitive search term and press Enter to apply it, close the dialog, and select the first result at or after the caret (wrapping to the start). `Esc` or `F5` closes Find without clearing its active term. Plain `F6` selects the previous result and `F7` selects the next result; both wrap and switch from preview to source only when a result exists. Empty Enter clears the active search. Search never leaves the active Markdown source document.

The three-column layout keeps the page centered in its middle workspace and provides physical page-edge resize handles. Tree, editor/preview, and outline have independent scrolling. Markdown preview, H1–H5 outline navigation, word totals with a daily baseline, incremental worker analysis, and five-minute autosave are included.

## Architecture and security boundary

The production editor is hosted by Tauri. CodeMirror 6 runs in the webview, while the Rust backend owns the opened project and all filesystem destinations. The `amte-gpui` binary is a deliberately separate GPUI boundary demonstration: GPUI has its own window and event loop and is not embedded in the Tauri editor window.

The frontend receives a canonical project display path and sends only relative entry paths. Tauri exposes purpose-specific picker, restore, list, open, save, Save As, and autosave commands; it exposes no generic path read/write command or frontend filesystem permission. On Linux, child components are resolved from a held project-directory descriptor with no-follow flags. Absolute, prefix, parent, hidden, ignored, non-Markdown, missing-directory, and symlink paths are rejected. Existing saves stage a new inode and atomically replace the project entry; Save As is no-clobber. Multiply linked targets are rejected, so existing external hardlink aliases are not modified. Opaque project and document identities prevent stale save or autosave results from writing after a project/document switch.

This is a desktop-project security model with an explicit cooperative same-UID assumption: it does not claim to stop another same-UID process that deliberately renames directories or changes hardlinks during the final namespace commit.

## Development, verification, and builds

Prerequisites: Node.js 20+ with npm, stable Rust/Cargo (the crate declares Rust 1.85), Chromium for the browser benchmark, and the Linux [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
npm ci

# Frontend development and verification
npm run dev
npm run lint
npm test
npm run search:browser
npm run build
npm run benchmark

# Rust verification
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml --all-targets
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings

# Release binary with embedded frontend
npm run build && cargo build --release --manifest-path src-tauri/Cargo.toml --bin amte
```

Direct `cargo run` and `cargo build` embed the generated `dist/` frontend and do not use localhost. `cargo tauri dev` uses the configured Vite development server. The release launcher binary is `src-tauri/target/release/amte`.

`npm run benchmark` serves an in-app 400 KiB Markdown measurement through Chromium CDP. CodeMirror remains virtualized, worker messages contain incremental transaction changes, project opening does not recursively enumerate the tree, and lazy tree patches preserve rows outside the changed branch.

## Platform limits

Project file access is intentionally supported only on Linux x86_64, where AMTE uses the `openat2` and `renameat2` syscall interface for its descriptor-bound path model. Other operating systems, and Linux architectures other than x86_64, are compile-time unsupported. The application has only been configured for the Linux Tauri prerequisite set.

## Themes, attribution, and font license

AMTE bundles the Monokai +Orange theme derived from [tw-studio/monokai-accent](https://github.com/tw-studio/monokai-accent); the upstream theme is MIT-licensed and credited in `src/theme.json`. Vintage MS-DOS Edit is an original AMTE treatment inspired by DOS text-editor conventions; its blue/gray stipple is a CSS gradient tile derived from its theme JSON and uses no screenshot pixels or runtime external assets.

The bundled VT323 Regular font is distributed unchanged under the SIL Open Font License 1.1. Its complete license is retained at `src/assets/fonts/OFL.txt`.
