# Rust tour through Twill

This guide explains Rust through the current Twill source. The project compiled and its initial unit-test run passed 7 tests. The guide describes source structure and ownership relationships; it does not claim that every runtime path has been exercised.

## Ownership and borrowing: one document, many views

In [`src/app.rs`](../src/app.rs), `Twill` owns `documents: HashMap<u64, Document>` and a `Layout`. Each pane owns tabs of `View` values, and each view stores a document ID. This lets split panes refer to the same document without each view owning a separate copy of its text.

When the app renders, `draw_layout` receives mutable references to the layout and document map. The editor obtains a mutable `Document` for the active view, then calls methods such as `replace`. Rust checks that these mutable borrows do not overlap illegally. This shapes the event flow: UI callbacks collect `Action` values, then the app processes those actions after drawing has finished.

The rope helps with text ownership too. `Highlighter::request` in [`src/syntax.rs`](../src/syntax.rs) takes a `Rope` clone and sends it to a worker. Rope clones share internal chunks, so the request does not first create a full duplicate `String` of the document.

## Enums make state explicit

`Layout` in `src/app.rs` is either a `Leaf(Pane)` or a `Split` containing two boxed layouts. Recursive layout is represented directly in the type, and `match` handles each shape when finding, splitting, removing, or drawing panes.

Other examples include `VimMode` (`Normal`, `Insert`, `Visual`, `VisualLine`) in [`src/vim.rs`](../src/vim.rs), and `Shell` (`PowerShell`, `CommandPrompt`, `Wsl`) in [`src/terminal.rs`](../src/terminal.rs). Matching on these enums makes supported states and choices visible to the compiler and reader.

The app's `Action` enum carries UI requests such as opening a path, closing a tab, saving, splitting, searching, or focusing a pane. Collecting these requests separates rendering from mutations that would otherwise conflict with active borrows.

## `Result` for file and process failures

`Document::open` returns `anyhow::Result<Self>` because reading a file or decoding UTF-8 can fail. `Document::save` and `reload` do the same. Callers decide where to show an error: `Twill::open` stores a message, while `save_id` displays a “Save failed” message and returns `false` so a close flow can remain open.

The `?` operator in `src/document.rs` propagates errors while adding context, for example identifying which path could not be read. This keeps the lower-level document code responsible for describing the failure and the UI responsible for presenting it.

## Traits define component boundaries

The application implements `eframe::App` for `Twill` in `src/app.rs`; the framework calls `update` for each UI frame. `Settings` derives Serde's `Serialize` and `Deserialize` traits in `src/platform.rs`, which lets TOML encode and decode its persisted fields. `Terminal` implements `Drop`, so its shutdown method is called when the terminal value is discarded.

These trait implementations connect Twill to framework and library behavior without requiring the app to control those libraries' internal loops or serialization formats.

## Threads and channels move background work

`Highlighter::new` creates standard-library channels and starts a named worker thread. The UI sends `Request` values through a `Sender`; the worker receives them and sends `LineResult` values back through a bounded `SyncSender`. The UI polls results without blocking. Each result carries a document ID and revision, allowing stale work to be discarded.

The terminal uses a separate reader thread in `src/terminal.rs`. The UI thread writes to the PTY, while the reader feeds bytes into a shared `vt100::Parser`. `Arc<Mutex<_>>` shares parser state between threads, and an `AtomicBool` communicates whether the reader is still alive. The UI uses `try_lock` so drawing does not wait for the parser lock.

These are real examples of Rust's thread-safety constraints: values crossing threads must meet the required `Send`/`Sync` bounds, and shared mutable state must use synchronization. They also show a practical tradeoff: bounded/nonblocking UI communication keeps frames responsive, while current lock contention or terminal latency has not yet been measured.

## Small tests exercise document invariants

Tests alongside `src/document.rs` check undo state, Unicode grapheme navigation, BOM/newline preservation, and external file modification detection. `src/vim.rs` tests counted line deletion and grouped insert undo. `src/platform.rs` tests settings serialization. These tests explain a few important invariants, but they do not cover the full UI, recovery, or interactive terminal behavior.
