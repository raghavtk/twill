# Design log

This log records decisions visible in the current implementation and calls out the evidence limits that remain. Source behavior is not automatically proof of every interactive Windows workflow.

## Toolchain and build

`build.ps1` runs Cargo from the repository. If `.tools/cargo/bin/cargo.exe` exists, it sets `CARGO_HOME`, `RUSTUP_HOME`, and `PATH` to use that local toolchain; otherwise it expects Rust on `PATH`. `.tools` is ignored and is not distributed in a clone. The script supports debug build, release build, and tests. From WSL, invoke it using `powershell.exe -NoProfile -File "$(wslpath -w ./build.ps1)" -Release`, then run `./target/release/twill.exe`. The desktop target is Windows; the current eframe setup does not provide native Linux X11 or Wayland support.

The current offline test run passed 37 tests with 0 failures and 0 ignored tests. Formatting, Clippy with `--all-targets -- -D warnings`, and `cargo build --release --offline` passed.

## Native UI: eframe and egui

`src/main.rs` starts `app::Twill` in a native window. `src/app.rs` draws line-numbered editor views with `egui::ScrollArea::show_rows`, so each frame renders the visible line range rather than placing the full document in one text widget. The app manages tabs, nested split panes, prompts, settings, terminal display, and file tree. This describes the implementation, not a measured frame-time or memory profile.

The editor keeps focus while handling navigation and editing keys, including arrows, Tab, and Escape. This matters because those events must reach the editor's own cursor and selection logic instead of being consumed by surrounding UI navigation.

## Document model: Ropey

`src/document.rs` stores text in `ropey::Rope`. Rope ranges let edits avoid rebuilding a contiguous document string for each keystroke. The editor uses character offsets; file and search operations preserve original UTF-8 byte positions as needed. Undo history uses a `VecDeque` and is limited to 16 MiB, accounting for each `Edit` record plus the capacities of its before and after strings.

Opening reads UTF-8, recognizes a UTF-8 BOM, records newline convention, and fingerprints the original bytes. Save checks for external changes, streams rope chunks to a temporary sibling file, calls `sync_all`, rechecks for external changes, and replaces the destination. Save As and forced overwrite have separate paths. Filesystem replacement behavior still depends on Windows and filesystem semantics, so the relevant Windows flows need runtime coverage.

The document tests cover saved-state undo/redo, grapheme navigation, BOM and newline preservation, external edit detection, and change-journal behavior. Do not infer broad file compatibility from these focused cases.

## Search and Unicode offsets

`src/search.rs` implements literal forward and backward search. Case-sensitive search uses `str::match_indices`. Case-insensitive search streams Unicode lowercase characters through a KMP matcher and keeps only query-sized origin data, mapping matches back to byte ranges in the original text. It avoids lowercasing the whole document, and rejects a match that would cover only part of one character's lowercase expansion. Tests cover expansion, contraction, repeated prefixes, wrapping, and literal matching.

The find bar requests focus for its query when opened from `Ctrl+F` or View > Find / replace. Enter in the query searches forward, Enter in the replacement field replaces, and Escape closes the bar whenever it is open, then returns focus to the editor. The find bar consumes Escape before the editor handles it. Search and replacement rebase stale view positions before creating or changing a match after edits in another pane. Replace uses a matching current selection or advances to the next match, replaces it, and selects the following match. Each replacement remains a separate undo edit. These actions are collected as a small `FindBarAction` enum and handled by the app.

## Syntax coloring: Syntect and two-face

`src/syntax.rs` processes highlighting on a worker thread and returns visible-line layout jobs through a bounded channel. Rope clones share their backing chunks. The rendered-line cache is bounded by 8 MiB of estimated job text and section storage, with FIFO eviction. The worker yields in quanta of up to 128 lines and clips rendered lines at 4,096 characters so a generated long line cannot monopolize highlighting. Documents above 10 MiB skip highlighting in the editor.

`build.rs` loads `assets/JSONC.sublime-syntax` and compiles it into the generated syntax pack used by the app. JSONC is therefore included in the bundled grammar, rather than depending on a machine-installed grammar. No syntax throughput or memory benchmark is recorded here.

## Terminal: portable-pty and vt100

`src/terminal.rs` uses `portable-pty` to launch PowerShell, Command Prompt, or WSL and `vt100` to parse screen control sequences. A reader thread processes shell output while the UI presents the parsed screen and sends input. The code recognizes split cursor-position query sequences across output chunks and writes the cursor response back to the shell. The terminal also supports resize, common keys, and bracketed paste.

Windows startup, shell interaction, resize, query replies, and shutdown are behaviors to validate through the Windows terminal integration test and interactive use. A helper test for parsing bytes alone would not establish that a real spawned shell receives the responses.

## Editing workflow and Vim subset

Tabs, nested split panes, a file tree, find/replace, and settings are managed in `src/app.rs`. The Vim subset in `src/vim.rs` has Normal, Insert, Visual, and VisualLine states; motions, counts, delete/change/yank operators, a register, undo/redo, and repeat-last-change. It is intentionally not full Vim compatibility.

Each `View` owns its cursor, selection anchor, Vim state, and scroll state. `Layout` owns pane structure. Documents live once in a map and views refer to them by ID, so a single document can appear in multiple panes. Filesystem notifications and app actions clear the file tree cache so directory changes can be reflected.

## Settings, external changes, and recovery

`src/platform.rs` stores settings and recovery data below `%LOCALAPPDATA%\twill`, falling back to the system temporary directory if the environment variable is unavailable. Settings use TOML; recovery snapshots use JSON and are written after an idle interval for dirty documents. Startup offers to restore or discard discovered snapshots.

Recovery writes use `RecoveryRef<'a>` to borrow the document path, newline convention, and rope while serialization runs. `RopeText` implements `Serialize` by calling `serializer.collect_str` on Ropey's `Display` implementation, letting `serde_json::to_writer` stream rope chunks through JSON string escaping into a `BufWriter` instead of first building a full text `String` and serialized `Vec<u8>`. The writer flushes and calls `sync_all` before the temporary file is renamed into place. On serialization or write failure, the temporary file is removed, so an earlier snapshot remains intact. Recovery is still synchronous on the UI thread, and its latency has not been measured. Tests cover escaped Unicode, control characters, CRLF, BOM metadata, and an injected serializer failure.

The app polls document fingerprints and watches parent directories. A clean externally changed document reloads; a dirty one prompts for reload, overwrite, or Save As. Undoing back to a saved revision retires its earlier dirty recovery snapshot. Forced-crash recovery, external-change choices, and competing-write handling still warrant direct interactive Windows checks.

## Update policy

When behavior or architecture changes, record the concrete problem, the implementation change, and the evidence that supports the claim. Keep source-level behavior separate from test or runtime evidence, and do not publish stale test counts or unmeasured performance claims.
