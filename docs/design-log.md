# Design log

This log explains implementation choices and records why they were made. It also distinguishes source-level behavior from work that still needs runtime or performance verification.

## Toolchain and first build

**Starting problem:** Rust was initially missing from `PATH` and the usual Cargo directory. The repository had no Rust source, so there was no build target to test.

**Resolution:** The project now has a local Rust toolchain under `.tools`, and `build.ps1` configures `CARGO_HOME`, `RUSTUP_HOME`, and `PATH` before invoking Cargo. The first build and test run passed 7 tests on the Windows development environment. The script supports debug build, release build, and tests.

**Remaining evidence gap:** A successful compile and unit-test run do not establish interactive UI behavior or memory usage. Those need runtime checks as features evolve.

## Native UI: eframe and egui

**Decision:** Use `eframe`/`egui` 0.31 for the desktop application and custom editor rendering.

**Reason:** Twill can own its editor surface, document interactions, and terminal presentation within one Rust application. The editor draws visible lines from the document rope through an `egui::ScrollArea::show_rows` view, instead of converting the entire document into a UI text widget each frame.

**Current evidence:** `src/main.rs` creates a resizable native window and starts `app::Twill`. `src/app.rs` implements menus, tabs, split layouts, status, dialogs, and the editor surface. Memory use has not been measured.

## Document model: Ropey

**Decision:** Store text in `ropey::Rope` in `src/document.rs`.

**Reason:** Editing can update rope ranges without rebuilding a single contiguous string for each keystroke. The document uses character offsets at the editor boundary and converts to bytes only where needed for file/search operations. Undo history is capped at 16 MiB of stored before/after edit text.

**Additional file behavior:** Opening reads UTF-8, recognizes and preserves a UTF-8 BOM, records the preferred newline convention, and fingerprints the original bytes. Save checks for external changes and writes through a temporary sibling file before replacing the target. This currently assumes the temporary file can be renamed over the target on the supported Windows filesystem; replacement behavior deserves direct Windows runtime verification.

**Tests present:** `src/document.rs` covers saved-state undo/redo, grapheme movement, BOM/newline preservation, and external edit detection.

## Syntax coloring: Syntect and two-face

**Decision:** Run Syntect highlighting with `two-face` syntax definitions in `src/syntax.rs`.

**Reason:** The UI thread should not parse a whole file while drawing. `Highlighter` sends a cloned Rope to a worker thread; Rope clones share underlying storage. The worker processes visible-line results into a bounded channel, and the UI drains results into an LRU-like bounded cache (up to 12,000 line entries). New document revisions replace queued work for that document as the worker notices them.

**Current evidence and limits:** The editor requests highlighting by document revision and file extension. It falls back to plain text if no syntax is found. The app disables highlighting for documents above 10 MiB. Highlighting latency and memory use are not benchmarked. Cache eviction order is FIFO, not a true recency-based LRU.

## Terminal: portable-pty and vt100

**Decision:** Use `portable-pty` to launch shells and `vt100` to parse terminal control sequences in `src/terminal.rs`.

**Reason:** Terminal output contains cursor and screen-control sequences, so interpreting it as plain text would not produce a usable terminal. A reader thread feeds output to the parser, while the UI draws the parsed screen and writes keyboard input to the PTY.

**Current scope:** PowerShell, Command Prompt, and WSL launch choices are present. The terminal resizes the PTY with its panel and supports common keys and bracketed paste. Shell-specific behavior and process shutdown need interactive Windows checks.

## Editing workflow and Vim subset

Tabs, nested split panes, a file tree, find/replace, and settings are implemented in `src/app.rs`. The Vim mode in `src/vim.rs` has Normal, Insert, Visual, and VisualLine states; motions, counts, delete/change/yank operators, a single register, undo/redo, and repeat-last-change are implemented. It is a deliberately limited subset, not full Vim compatibility.

Per-view cursor, selection anchor, Vim state, and scroll reveal state live in a `View`. `Layout` owns panes and split structure. Documents are stored once in a map and referenced by views, so a document can appear in more than one pane.

## Settings, external changes, and recovery

`src/platform.rs` stores settings and recovery data below `%LOCALAPPDATA%\twill` (falling back to the system temporary directory if the environment variable is unavailable). Settings are TOML. Recovery snapshots are JSON and are written after an idle interval when a dirty document's revision changes. Startup offers to restore or discard discovered snapshots.

The app polls document fingerprints and also watches parent directories. A clean externally changed document reloads; a dirty one gets a conflict prompt with reload, overwrite, and save-as choices. Recovery and conflict flows are implemented but need forced-crash and external-edit runtime checks before their reliability is established.

## Known UX issue from source review

The file tree caches directory entries. The cache is cleared when a folder is first opened, but the current watcher path only checks open documents and does not invalidate tree entries after files are added or removed. A tree can therefore stay stale until the cache is cleared. Root has been told; this log should be updated when the refresh behavior changes.

## Update policy

When behavior or architecture changes, update the relevant decision with the concrete problem, its effect, the chosen change, and the evidence that resolved it. Do not promote intended behavior to verified behavior without a build or runtime check appropriate to the claim.
