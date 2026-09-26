# Rust tour through Twill

`src/caret.rs` shares a small `Blink` state machine between the editor and terminal. Its `update` method returns a tuple containing visibility and the delay until the next transition. egui stores one state per widget ID, so each view keeps independent timing. `request_repaint_after` schedules the next blink without a rendering loop. The timing test supplies explicit times rather than sleeping.

This guide explains Rust through the current Twill source. It describes code structure and selected invariants, not proof that every interactive path has been exercised.

## Ownership and borrowing: one document, many views

In [`src/app.rs`](../src/app.rs), `Twill` owns a `HashMap<u64, Document>` and a `Layout`. Each pane owns tabs of `View` values, and each view stores a document ID. Split panes can therefore show the same document without owning separate text copies.

When the app draws, it passes mutable references to layout and documents. The editor gets the active `Document` and calls methods such as `replace`. Rust checks that these mutable borrows do not overlap illegally. UI callbacks collect `Action` values, and the app processes those actions after drawing, which keeps structural changes outside the active borrows.

The document text is a `ropey::Rope`. `Highlighter::request` in [`src/syntax.rs`](../src/syntax.rs) takes a Rope clone to send to a worker. Rope clones share their underlying chunks, so this does not first create a full document `String`.

## Enums make state explicit

`Layout` in `src/app.rs` is either a `Leaf(Pane)` or a `Split` with two boxed layouts. This recursive type models nested panes directly. `match` handles each shape when searching, splitting, removing, or drawing panes. The command prompt supports `w`, `q`, `q!`, `wq`, and line-number navigation; splits are created through the View menu.

Other examples include `VimMode` (`Normal`, `Insert`, `Visual`, `VisualLine`) in [`src/vim.rs`](../src/vim.rs), and `Shell` (`PowerShell`, `CommandPrompt`, `Wsl`) in [`src/terminal.rs`](../src/terminal.rs). The app's `Action` enum carries requests such as opening a path, saving, splitting, searching, or focusing a pane. Explicit variants make these supported states visible in the code.

## Retaining a vertical cursor column

Each `View` stores `preferred_column: Option<usize>`. `move_vertical` initializes it with `Option::get_or_insert_with`, using the cursor's character offset from the current line start. Later Up, Down, Page Up, or Page Down movements reuse that value even when a short line clamps the visible cursor closer to the line start. This avoids losing the intended column while traversing uneven lines.

The stored column counts Unicode scalar values, not tab-expanded columns or rendered pixels. On a destination line, the cursor is snapped down to a grapheme boundary with `prev_grapheme`, so it will not land inside a combining sequence or emoji cluster. Horizontal keys, edits, pointer placement, searches, and document revision changes reset the saved column. With Vim disabled, Left and Right collapse a selection to its start or end respectively, independent of which direction the selection was made, without moving one more character.

## `Result` for file and process failures

`Document::open`, `save`, and `reload` return `anyhow::Result` because reading, decoding, writing, or replacing a file can fail. Callers decide how to present errors. The `?` operator propagates failures while adding path context, keeping lower-level file details close to the operation and UI messages in the app.

## Traits define component boundaries

`Twill` implements `eframe::App` in `src/app.rs`; the framework calls `update` for each frame. `Settings` derives Serde's `Serialize` and `Deserialize` traits in `src/platform.rs`, so TOML can encode and decode its persisted fields. `Terminal` implements `Drop`, so it shuts down its child process when discarded.

## Threads and channels move background work

`Highlighter::new` creates channels and starts a named worker thread. The UI sends `Document`, `Visible`, and `Forget` messages; the worker returns `LineResult` values through a bounded synchronous channel. Results carry a document ID and revision, which lets the UI discard stale highlighting after edits. The rendered-line cache is capped at 8 MiB of estimated text and section storage. The worker processes work in batches and limits each rendered line to 4,096 characters.

The terminal has a separate reader thread in `src/terminal.rs`. It reads from the PTY, updates a shared `vt100::Parser`, and detects cursor-position queries even when a query arrives split across output chunks. The app writes the terminal response back to the PTY. `Arc<Mutex<_>>` shares parser state; the UI uses `try_lock` while drawing so it does not wait for the reader's lock.

These are examples of Rust's thread-safety rules: data crossing threads must satisfy `Send` and `Sync` requirements, and shared mutable state needs synchronization. Channels and nonblocking locks help keep UI work responsive, but no latency measurements are documented here.

## Literal search and byte offsets

`find_match` in [`src/search.rs`](../src/search.rs) returns byte ranges into the original UTF-8 text. Case-sensitive search uses `match_indices`. Case-insensitive search uses a streaming KMP matcher over Unicode lowercase characters and keeps a query-sized queue mapping folded characters to source byte offsets. This avoids allocating a lowercased copy of the whole document. It also avoids returning a partial range when one source character lowercases to multiple characters.

In `src/app.rs`, `find_bar` uses an optional `FindBarAction` enum to return search, replace, or close requests from the UI callback. Opening the bar requests focus on the query. Enter searches from the query field or replaces from the replacement field. Escape closes the bar whenever it is open and is consumed before the editor handles it, which matters because egui can clear text-field focus before the frame processes the key. Search and replacement rebase stale view positions after edits in another pane. A replacement first accepts a matching selection or finds and replaces the next occurrence from the cursor, then selects the following match. Each replacement is its own undo edit. A UI test checks query focus, typing, Enter, and Escape handling.

## File writes and saved state

`Document::save` streams rope chunks into a temporary sibling file instead of first flattening the rope into one large string. It syncs the temporary file, checks the destination fingerprint again for external changes, and then replaces the destination. The document tracks saved state separately from edit revisions, so undoing back to the saved state clears the dirty marker. The app also retires an obsolete recovery snapshot when undo returns to that state.

Recovery serialization in [`src/platform.rs`](../src/platform.rs) shows how lifetimes and traits work together. `RecoveryRef<'a>` borrows the path, newline string, and `Rope` from a document for the duration of the write. Its `RopeText<'a>` wrapper implements Serde's generic `Serialize` trait by calling `serializer.collect_str` with Ropey's `Display` implementation. With `serde_json::to_writer`, that streams text fragments through JSON escaping into a buffered file writer without allocating one full flattened text string and a second serialized byte vector. After flush and `sync_all`, the completed temporary file replaces the prior snapshot. If serialization fails, cleanup removes the temporary file before rename, preserving the previous snapshot. Tests include Unicode and control-character escaping, CRLF text, BOM metadata, and a serializer that deliberately fails. Snapshot writing still runs synchronously on the UI thread, and its latency has not been measured.

Undo edits live in a `VecDeque<Edit>`. Each edit's budget includes the `Edit` struct itself and the allocated capacities of its before and after strings. Old records are evicted from the front while the 16 MiB limit is exceeded, retaining at least one edit.

The Vim implementation also shows why movement and text ranges need line and grapheme boundaries. Counted `x` removes up to the current line end by repeatedly calling `next_grapheme`, so it does not consume a newline or split a combined character. Append (`a`) advances only as far as the current line end, including for an empty line. Leaving Insert with Escape moves back one grapheme but clamps that move at the current line's start, preventing it from crossing the preceding newline.

## Tests exercise focused invariants

Unit tests in `src/document.rs` cover saved-state undo, grapheme navigation, BOM and newline preservation, external edit detection, and the change journal. `src/search.rs` tests Unicode offset mapping, repeated prefixes, wrapping, and literal matching. `src/terminal.rs` tests key encoding, colors, and cursor-query parsing; a Windows-only integration test starts a real Command Prompt through `Terminal::spawn` and checks output, resize, and shutdown. `src/platform.rs` tests recovery round trips and failure cleanup. `src/syntax.rs` checks the bundled syntax set, rendered line breaks, and worker result handling. These focused tests do not cover every interactive UI workflow, recovery scenario, or shell configuration.
