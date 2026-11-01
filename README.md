# AMTE

AMTE is a local-first Markdown project editor for focused writing. Its desktop editor is a Tauri application with a CodeMirror 6 source buffer, a rendered Markdown preview, live outline and word analysis, and offline bundled themes.

## Project workflow

- `Ctrl+O` opens a native **folder** picker. AMTE keeps one opened project; the title bar presents its canonical path (or `No project open`).
- The project tree is lazy: opening a project reads its root, and expanding a directory reads that directory once. Directories appear before `.md` and `.markdown` files in stable case-insensitive order. Hidden entries, `.git`, `node_modules`, `target`, `dist`, and symlinks are omitted.
- Selecting another Markdown file in the same project switches immediately. AMTE keeps one in-memory CodeMirror `EditorState` per opened file while reusing one EditorView and one analysis worker, so text, undo/redo, selection, source/preview/chapter scroll, preview mode, Find term, outline state, metrics, and dirty state remain per-file for the running session. Undo history is intentionally rebuilt after a restart.
- Changing projects asks once for **Save All**, **Discard All**, or **Cancel** when any buffer is dirty. Save All attempts every dirty eligible buffer even if another fails; a failed/conflicted/untitled buffer, or a committed file still awaiting durable reconciliation, keeps the old session selected. File switching never prompts.
- `Ctrl+S` saves the active file. With an open project but no selected file, it opens Save As and accepts a new `.md` file only in an existing directory inside that project. Untitled text can be edited without a project but cannot be saved until one is opened.
- Project state is stored atomically as `last-project-v1`, with a canonical root and optional relative file. A stale file restores its project without opening a document; a stale root is discarded. A valid legacy `last-document-path` is migrated once.

## Keyboard navigation and editing

- `Ctrl+O`: open project folder; `Ctrl+S`: save; `Ctrl+P`: source/preview; `Ctrl+T`: toggle theme.
- `Ctrl++` / `Ctrl+-`: application zoom; `Ctrl+[` / `Ctrl+]`: page width; `Ctrl+Backspace`: reset page width.
- Plain `F1` focuses the selected (or first) project-tree item, `F2` focuses CodeMirror without changing its caret or selection, and `F3` focuses the current visible (or nearest visible ancestor/first) outline heading. The H1–H6 outline is a collapsible tree: Up/Down, Home/End move and navigate visible headings; Right expands or enters a child; Left collapses or moves to a parent; Enter navigates; Space toggles a heading with descendants. The disclosure button only expands/collapses, while the heading title navigates to its exact source or preview occurrence.
- Plain `F5` opens Find in document. Type a literal, case-insensitive search term and press Enter to apply it, close the dialog, and select the first result at or after the caret (wrapping to the start). `Esc` or `F5` closes Find without clearing its active term. Plain `F6` selects the previous result and `F7` selects the next result; both wrap and switch from preview to source only when a result exists. Empty Enter clears the active search. Search never leaves the active Markdown source document.
- `src/config.json` configures editor behavior. `editor.autoClosePairs` defaults to `true` (including when missing or malformed) and enables CodeMirror pair completion for `()`, `[]`, `{}`, `""`, `''`, and backticks.

The three-column layout keeps the page centered in its middle workspace and provides physical page-edge resize handles. Tree, editor/preview, and outline have independent scrolling. Markdown preview, H1–H6 outline navigation with per-document collapsed state, live whole-document totals, per-file daily gross word activity (`(-): deleted (+): added`), incremental worker analysis, and five-minute autosave are included. Activity is local-calendar-day scoped, persists per saved file, and treats undo/redo as new editing activity.

## Architecture and security boundary

The production editor is hosted by Tauri. CodeMirror 6 runs in the webview, while the Rust backend owns the opened project and all filesystem destinations. The `amte-gpui` binary is a deliberately separate GPUI boundary demonstration: GPUI has its own window and event loop and is not embedded in the Tauri editor window.

The frontend receives a canonical project display path and sends only relative entry paths. Tauri exposes purpose-specific picker, restore, list, open, save, Save As, autosave, revalidation, and recovery commands; it exposes no generic path read/write command or frontend filesystem permission. Every opened document has an opaque ID, strictly increasing revision, held file identity, and BLAKE3 content baseline. Saves (including inactive-buffer autosaves) recheck the identity and bytes before writing; an external change, replacement, missing target, symlink, or hardlink produces a conflict and is never overwritten. Keep/copy the in-memory text, then explicitly discard/reload it; AMTE has no merge or force-overwrite UI.

Crash recovery is backend-managed under the Tauri per-user config directory in the experimental V2 SQLite/WAL store, not localStorage or a manuscript directory. Rust owns durable snapshot IDs/sequences and retained revisions; CodeMirror remains the sole live editor owner and Rust owns manuscript saves. Dirty snapshots are debounced (750 ms trailing, five-second maximum), while five-minute autosave is a separate file-write lifecycle. Recovery content is plaintext manuscript text: protect the account/config directory accordingly. Power loss/SIGKILL can lose edits made after the last completed snapshot; full disks, unsafe permissions, malformed/future databases, and hostile same-UID filesystem interference fail safely rather than claiming latest-edit recovery. See `docs/reliability-v2.md` before using the experimental branch.

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
