# personal-project-WOLFRAMSCRIPT-2

WolframScript but better

## Usage

Start the interactive REPL. By default this uses the native WSTP backend and keeps a kernel session alive for REPL state:

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

Build without WSTP and use subprocess evaluation instead:

```sh
cargo run --no-default-features -- -e '1+1'
```

The subprocess build still runs script files through `wolframscript`, but expression evaluation does not preserve REPL state between inputs.

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

By default the REPL also initializes Wolfram FrontEnd services in the background when they can be discovered. This is used as the boundary for FrontEnd-backed functionality such as special argument completions and future graphics rendering without opening a notebook window. If the FrontEnd cannot be initialized, completion falls back to the kernel-only engine.

FrontEnd special argument completions are loaded from readable Wolfram installation files that register `FEPrivate`AddSpecialArgCompletion`. Color, file, and directory completion classes are surfaced as typed suggestions with rudimentary styling. These definitions are private Wolfram APIs, so coverage can vary by Wolfram version and installation layout.

Disable FrontEnd integration and use the simpler kernel-only completion engine with:

```sh
cargo run -- --no-frontend
```

## REPL Commands

Lines that start with `:` are handled by the CLI instead of being evaluated as Wolfram Language input:

```text
:help
:theme
:theme dark|light|solarized|gruvbox|monokai|plain
:theme list
:theme show
:quit
```

`:theme` cycles the syntax highlighting theme. `:theme list` previews available themes. `:quit` exits the REPL; `Exit`, `Quit`, and Ctrl-D are also supported.

Command completions are available only when the line starts with `:`. Wolfram Language completions are disabled for those command lines.

## Kernel Discovery

Set `WOLFRAM_KERNEL` to override the kernel executable. Without that override, the CLI asks `wolframscript -showkernels` for the best local kernel path, falls back to `wolfram-app-discovery`, and prefers the native kernel binary under `SystemFiles/Kernel/Binaries` before falling back to `WolframKernel` on `PATH`.

Set `WOLFRAM_FRONTEND` to override the FrontEnd executable used for FrontEnd-backed completions and rendering support.

The `wstp` crate must be able to discover Wolfram's WSTP SDK for the default build. If that is not available, use `cargo run --no-default-features` for the subprocess fallback.

## Release Builds

GitHub Actions builds packaged binaries for Linux, macOS, and Windows when a `v*` tag is pushed or the release workflow is run manually. Release artifacts are built with `--no-default-features` so CI does not need a Wolfram installation or WSTP SDK.

The packaged binary locates the user's Wolfram installation at runtime using the discovery behavior above. Script files still require `wolframscript` on `PATH` because they are delegated to Wolfram's script runner.
