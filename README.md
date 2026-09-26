# Twill

Twill is a native Windows text editor written in Rust with `eframe` and `egui`. It combines a rope-backed editor, syntax coloring, a limited Vim mode, a file tree, split panes, recovery snapshots, and an embedded Windows pseudoterminal. WSL is available as a terminal shell; Twill is not a native Ubuntu/Linux desktop application.

## Build

From PowerShell in the repository, run:

```powershell
.\build.ps1
.\build.ps1 -Test
.\build.ps1 -Release
```

The script uses `.tools` when that local Rust toolchain is present, otherwise it uses the Rust installation on `PATH`. `.tools` is ignored by Git and is not included in a fresh clone, so install Rust locally or bootstrap that toolchain before building. To build the Windows app from WSL, call the PowerShell script through Windows interop:

```bash
powershell.exe -NoProfile -File "$(wslpath -w ./build.ps1)" -Release
./target/release/twill.exe
```

Do not use `cargo run` from native WSL for the desktop app: the current `eframe` build does not include Linux X11 or Wayland support.

## Use

The main shortcuts include `Ctrl+N` for a new document, `Ctrl+O` to open, `Ctrl+S` to save, `Ctrl+Shift+S` for Save As, `Ctrl+F` to open and focus Find / replace, `Ctrl+G` to open the command prompt, `Ctrl+W` to close the current tab, `Ctrl+Tab` to switch tabs, and ``Ctrl+` `` to show or hide the terminal. In Find / replace, Enter in the query finds the next match, Enter in the replacement field replaces, and Escape closes the bar. Replace uses a matching selection or finds the next match, replaces it, then selects the following match. Each replacement is a separate undo edit. Up, Down, Page Up, and Page Down keep the same character column across short lines while snapping to grapheme boundaries. The command prompt accepts `:w`, `:q`, `:q!`, and `:wq`, plus a line number to navigate. Use the View menu to create a split. The Vim mode implements a subset of modes, motions, operators, registers, and repeat behavior, not full Vim compatibility.

The terminal menu offers PowerShell, Command Prompt, and WSL. WSL requires a working WSL installation. Settings and recovery data are stored under `%LOCALAPPDATA%\twill` when that environment variable exists.

## Implementation notes

File tabs share one rounded surface with an unboxed close icon. They expand up to 208 logical pixels when space permits and scroll when crowded. Folder expanders use drawn chevrons. Editor and terminal cursors blink and reset on input or focus.

Documents use Ropey, bounded undo history, UTF-8 BOM and newline preservation, external-change checks, and crash recovery snapshots. Syntax work runs on a worker thread; its rendered-line cache is capped at 8 MiB, and the bundled grammar pack includes JSONC. Highlighting is skipped for documents over 10 MiB. These are implementation limits, not benchmark results.

See the [design log](docs/design-log.md) for decisions and known limits, and the [Rust tour](docs/rust-tour.md) for examples tied to the current source.
