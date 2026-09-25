# Twill

Twill is a native Windows text editor written in Rust with `eframe` and `egui`. It has a document model, a custom editor view, syntax coloring, a Vim-style command subset, and an embedded pseudoterminal. The current implementation is an early build; see the status below for what has and has not been verified.

## Build

The project includes a local Rust toolchain under `.tools`. From PowerShell, run:

```powershell
.\build.ps1
```

Use `.uild.ps1 -Test` to run the Rust unit tests, or `.uild.ps1 -Release` to create a release build. The script sets `CARGO_HOME`, `RUSTUP_HOME`, and `PATH` for the bundled toolchain. The first build passed 7 tests on the Windows development environment. The application has compiled; interactive runtime behavior and memory use have not yet been characterized.

## Current implementation

- Documents use Ropey and track edits, bounded undo history, UTF-8 BOMs, newline conventions, and changes made outside the editor.
- The editor renders line-numbered views and supports cursor movement, text entry, selection, clipboard operations, find/replace, tabs, split panes, and a file tree.
- Syntax coloring runs on a worker thread using Syntect and `two-face` grammars. Large files over 10 MiB skip syntax work in the editor view.
- The Vim mode includes a subset of modes, motions, operators, registers, and repeat-last-change behavior. This is not full Vim compatibility.
- The terminal uses `portable-pty` and `vt100`, with PowerShell, Command Prompt, and WSL launch choices. Shell processes and terminal parsing run outside the UI thread.
- Settings and recovery data are stored under `%LOCALAPPDATA%\twill` when `LOCALAPPDATA` is available.

These are source-level capabilities. Recovery, terminal behavior, and common editing workflows still need broader runtime verification. The editor is local and does not connect to remote services.

## Documentation

- [Design log](docs/design-log.md): implementation decisions, tradeoffs, known gaps, and setup history.
- [Rust tour](docs/rust-tour.md): Rust concepts explained through the current source code.
