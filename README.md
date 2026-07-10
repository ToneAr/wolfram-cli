# Wolf Shell

`wolfsh` is a beta Rust CLI for running Wolfram Language from the terminal with a persistent WSTP-backed REPL, one-shot expression evaluation, script delegation through `wolframscript`, and dynamic completions.

## Installation

Cross-platform WolframScript installer (requires `wolframscript`):

```sh
wolframscript -file https://raw.githubusercontent.com/ToneAr/wolfsh/main/installers/install.wls 
```

To pin a specific beta release tag, add `--version v0.5.0`.

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/ToneAr/wolfsh/main/installers/install.wls -OutFile install.wls
wolframscript -script .\install.wls
```

To pin a specific beta release tag, add `--version v0.5.0`.

Platform-specific installers are also available. Omit the version option to install the latest GitHub release, or pass `--version v0.5.0` / `-Version v0.5.0` to install this beta explicitly.

Linux/macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/ToneAr/wolfsh/main/installers/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/ToneAr/wolfsh/main/installers/install.ps1 | iex
```

## Usage

`wolfsh` has three user-facing execution modes:

| Mode | Command | Backend |
| --- | --- | --- |
| Interactive REPL | `wolfsh` or `cargo run` | Native WSTP session |
| One-shot expression | `wolfsh -e 'Range[5]^2'` | Native WSTP session |
| Script file | `wolfsh script.wls -- arg1` | Delegated to `wolframscript` |

For a detailed architecture and evaluation pipeline walkthrough, including WSTP packet flow diagrams, see [`docs/Architecture.md`](docs/Architecture.md).

Start the interactive REPL. This uses the native WSTP backend and keeps a kernel session alive for REPL state:

```sh
cargo run
```

Evaluate one expression and exit:

```sh
cargo run -- -e 'Range[5]^2'
```

Run a script file through `wolframscript`:

```sh
cargo run -- path/to/script.wls -- arg1 arg2
```

## Local Rust REPL with evcxr

Start a project-configured Rust REPL for trying crate-visible helpers and Wolfram calls directly:

```sh
./evcxr-local.sh
```

The launcher builds the project, points `evcxr` at `.evcxr/evcxr/init.evcxr`, and loads the modules in `src/` into the REPL. After startup you can call crate-visible items directly, for example:

```rust
parse_repl_command(":help")
wolfram_string_literal("Range[5]")
let mut kernel = KernelClient::new()?;
kernel.query_string("Range[5]^2")?
```

Use `:quit` to leave the REPL.

## Completion

The REPL opens an IDE-style completion popup dynamically as you type symbol characters. Use `Tab` to cycle/accept entries, `Shift+Tab` to move backward, and `Esc` to close the popup.

Symbol completions are queried from the active kernel session as you type, so user-defined symbols, functions, and loaded package symbols are included after each evaluation. The query uses prefix-shaped `Names` calls, for example:

```wl
Names[StringJoin[ prefix, "*"]]
```

Matching context names are suggested from `Contexts[]`, and qualified input such as `MyContext`My` queries symbols inside that context.

When the cursor is inside a function call after the first top-level comma, option completions are loaded lazily from:

```wl
Options[head]
```

For example, `Plot[x, {x, 0, 1}, PlotR` can complete `PlotRange`.

By default the REPL also initializes Wolfram FrontEnd services in the background when they can be discovered. This is used as the boundary for future FrontEnd-backed functionality such as graphics rendering without opening a notebook window. If the FrontEnd cannot be initialized, the REPL continues with the kernel-only engine.

Disable FrontEnd integration and use the simpler kernel-only completion engine with:

```sh
cargo run -- --no-frontend
```

Disable ANSI coloring with:

```sh
cargo run -- --no-color
```

## REPL Commands

Lines that start with `:` are handled by the CLI instead of being evaluated as Wolfram Language input:

```text
:clear
:help
:theme
:theme dark|light|solarized|gruvbox|monokai|plain
:theme list
:theme show
:quit
```

`:clear` clears the console. `:theme` cycles the syntax highlighting theme. `:theme list` previews available themes. `:quit` exits the REPL; `Exit`, `Quit`, and Ctrl-D are also supported.

Command completions are available only when the line starts with `:`. Wolfram Language completions are disabled for those command lines.

## Kernel Discovery

Set `WOLFRAM_KERNEL` to override the kernel executable. Without that override, the CLI asks `wolframscript -showkernels` for the best local kernel path, falls back to `wolfram-app-discovery`, and prefers the native kernel binary under `SystemFiles/Kernel/Binaries` before falling back to `WolframKernel` on `PATH`.

Set `WOLFRAM_FRONTEND` to override the FrontEnd executable used for FrontEnd-backed completions and rendering support.

The `wstp` crate links Wolfram's WSTP static library at build time. A build machine must have a Wolfram installation or WSTP SDK for the Rust target being built. If discovery does not find it, set `WSTP_COMPILER_ADDITIONS_DIRECTORY` to the target's `SystemFiles/Links/WSTP/DeveloperKit/<SystemID>/CompilerAdditions` directory.

Runtime expression evaluation requires WSTP; there is no subprocess fallback.

## Release Builds

See [`docs/BETA_RELEASE.md`](docs/BETA_RELEASE.md) for the beta release notes, install examples, known limitations, and release verification checklist.

GitHub Actions builds packaged binaries when a `v*` or `build*` tag is pushed. `test*` tags and manual workflow runs exercise the build/test path without packaging or publishing artifacts, unless the manual run is explicitly started from a `v*` or `build*` tag ref.

Release builds run on GitHub-hosted runners. Because GitHub-hosted runners do not include Wolfram and `wstp-sys` links the target WSTP static library during `cargo build`, the workflow extracts the required `CompilerAdditions` from official Wolfram Engine artifacts before building:

| Artifact         | Runner           | Rust target                | WSTP source                 |
| ---------------- | ---------------- | -------------------------- | --------------------------- |
| `linux-x86_64`   | `ubuntu-latest`  | `x86_64-unknown-linux-gnu` | Wolfram Engine Docker image |
| `macos-x86_64`   | `macos-15-intel` | `x86_64-apple-darwin`      | Wolfram Engine macOS DMG    |
| `macos-aarch64`  | `macos-15`       | `aarch64-apple-darwin`     | Wolfram Engine macOS DMG    |
| `windows-x86_64` | `windows-latest` | `x86_64-pc-windows-msvc`   | Wolfram Engine Windows MSI  |

Locally, set `WSTP_COMPILER_ADDITIONS_DIRECTORY` if automatic discovery does not find the target's `SystemFiles/Links/WSTP/DeveloperKit/<SystemID>/CompilerAdditions` directory. Linux builds also need the system `uuid` library available for linking, for example the `uuid-dev` package on Debian/Ubuntu systems.

The packaged binary locates the user's Wolfram installation at runtime using the discovery behavior above. Expression, REPL, and completion evaluation run over WSTP; script files are delegated to `wolframscript`.

## Regenerating build-time kernel data

Builds embed pre-generated kernel data from files committed under `build_tools/`; they do not launch `WolframKernel` during `cargo build`. When the generated data needs to be refreshed, run:

```sh
build_tools/generate-kernel-data.sh
```

Set `WOLFRAM_KERNEL=/path/to/WolframKernel` to force a specific kernel. The script currently regenerates `build_tools/builtin_symbols.tsv`; commit that file with the source change that requires the refresh.
