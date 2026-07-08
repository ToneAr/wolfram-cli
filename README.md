# Wolfram CLI
## Usage

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



## Completion

The REPL opens an IDE-style completion popup dynamically as you type symbol characters. Use `Tab` to cycle/accept entries, `Shift+Tab` to move backward, and `Esc` to close the popup.

Symbol completions are queried from the active kernel session as you type, so user-defined symbols, functions, and loaded package symbols are included after each evaluation. The query uses prefix-shaped `Names` calls, for example:

```wl
Names[prefix <> "*"]
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

GitHub Actions builds packaged binaries for Linux, macOS, and Windows when a `v*` or `build*` tag is pushed. `test*` tags and manual workflow runs exercise the build/test path without packaging or publishing artifacts, unless the manual run is explicitly started from a `v*` or `build*` tag ref.

Release builds require self-hosted runners with Wolfram/WSTP SDKs installed because GitHub-hosted runners do not include Wolfram and `wstp-sys` links the target WSTP static library during `cargo build`. Each release target runs on a self-hosted runner for that OS/architecture, labeled `wolfram-wstp` in addition to the standard GitHub runner labels.

| Artifact | Required runner labels | Rust target | WSTP SystemID directory |
| --- | --- | --- | --- |
| `linux-x86_64` | `self-hosted`, `Linux`, `X64`, `wolfram-wstp` | `x86_64-unknown-linux-gnu` | `Linux-x86-64` |
| `linux-aarch64` | `self-hosted`, `Linux`, `ARM64`, `wolfram-wstp` | `aarch64-unknown-linux-gnu` | `Linux-ARM64` |
| `macos-x86_64` | `self-hosted`, `macOS`, `X64`, `wolfram-wstp` | `x86_64-apple-darwin` | `MacOSX-x86-64` |
| `macos-aarch64` | `self-hosted`, `macOS`, `ARM64`, `wolfram-wstp` | `aarch64-apple-darwin` | `MacOSX-ARM64` |
| `windows-x86_64` | `self-hosted`, `Windows`, `X64`, `wolfram-wstp` | `x86_64-pc-windows-msvc` | `Windows-x86-64` |

Before starting each self-hosted runner service, export `WSTP_COMPILER_ADDITIONS_DIRECTORY` to the matching target directory, for example:

```sh
export WSTP_COMPILER_ADDITIONS_DIRECTORY=/usr/local/Wolfram/Mathematica/14.1/SystemFiles/Links/WSTP/DeveloperKit/Linux-x86-64/CompilerAdditions
```

Linux runners also need the system `uuid` library available for linking, for example the `uuid-dev` package on Debian/Ubuntu systems.

The packaged binary locates the user's Wolfram installation at runtime using the discovery behavior above. Expression, REPL, and completion evaluation run over WSTP; script files are delegated to `wolframscript`.
