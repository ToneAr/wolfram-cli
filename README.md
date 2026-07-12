# Wolfie - **Wol**_fram_ **f**_riendly_ **i**_nteractive_ _sh_**e**_ll_

![logo](docs/img/logo.gif)

`wolfie` is a Rust CLI for running Wolfram Language from the terminal with a
WSTP-backed REPL, one-shot expression evaluation, script execution, and dynamic
completions.

Script execution runs through the same WSTP connection machinery as expression
and REPL evaluation, so it can use `--linkconnect` setups for persistent
kernels.

The TUI is self contained within the single `wolfie` binary and does not bundle the Wolfram Kernel.

## Installation

To pin a specific beta release tag, add `--version v0.5.0`.

Platform-specific installers are also available. Omit the version option to install the latest GitHub release, or pass `--version v0.5.0` / `-Version v0.5.0` to install this beta explicitly.

### WolframScript (All platforms)

```sh
wolframscript -script https://raw.githubusercontent.com/ToneAr/wolfie/main/installers/install.wls
```

### Bash (Unix)

```sh
curl -fsSL https://raw.githubusercontent.com/ToneAr/wolfie/main/installers/install.sh | bash
```

### PowerShell (Windows)

```powershell
irm https://raw.githubusercontent.com/ToneAr/wolfie/main/installers/install.ps1 | iex
```

## Usage

`wolfie` has three user-facing execution modes:

| Mode                | Command                          | Backend             |
| ------------------- | -------------------------------- | ------------------- |
| Interactive REPL    | `wolfie` or `cargo run`          | Native WSTP session |
| One-shot expression | `wolfie -e 'Range[5]^2'`         | Native WSTP session |
| Script file         | `wolfie --file script.wls -- a1` | Native WSTP session |

For a detailed architecture and evaluation pipeline walkthrough, including WSTP packet flow diagrams, see [`docs/Architecture.md`](docs/Architecture.md).

Start the interactive REPL. This uses the native WSTP backend and keeps a kernel session alive for REPL state:

```sh
wolfie
```

Use `--no-welcome` to skip the welcome banner while keeping normal input/output prompts, or `--no-prompt` to hide all REPL prompts and the welcome banner:

```sh
wolfie --no-welcome
wolfie --no-prompt
```

Evaluate one expression and exit:

```sh
wolfie -e 'Range[5]^2'
```

Pass WSTP link mode and link options through to a kernel launched by `wolfie`:

```sh
wolfie --linkmode Listen --linkoptions 4
wolfie --linkmode Listen --linkoptions 4 -e 'Range[5]^2'
```

Connect to an existing WSTP link for the REPL, one-shot evaluation, or script
execution instead of launching a new kernel. The link protocol defaults to
`SharedMemory`; pass `--linkprotocol` for `TCPIP` or `IntraProcess` links:

```sh
wolfie --linkconnect --linkname my-link
wolfie --linkconnect --linkname my-link --linkprotocol TCPIP
wolfie --linkconnect --linkname my-link --linkoptions 4 --linkinit
wolfie --linkconnect --linkname my-link -e 'Range[5]^2'
wolfie --linkconnect --linkname my-link --file script.wls
```

Run a script file over WSTP:

```sh
wolfie --file path/to/script.wls -- arg1 arg2
wolfie -f path/to/script.wls -- arg1 arg2
```

On Unix-like systems, `wolfie` can also be used directly as a shebang evaluator:

```wl
#!/usr/bin/wolfie
x = 2 + 2

y = x + 10

y
```

The script source is split into top-level Wolfram Language expressions by the
kernel reader and evaluated sequentially in one WSTP session. Only the final
expression result is returned, while earlier expressions share state with later
ones.

## TL;DR

Quick list of all features:

1. **Symbol completion**

   ![symbols](docs/img/symbols.png)

2. **Context completion**

   ![contexts](docs/img/contexts.png)

3. **Fuzzy matching** **[WIP]**

   ![fuzzy](docs/img/fuzzy.png)

4. **File and directory autocomplete**

   ![filesystem](docs/img/filesystem.png)

5. **System command completion (:)**

   ![commands](docs/img/commands.png)

6. **Quick shell mode (:!)**

   ![shell](docs/img/shell.png)

## Completion

### Details

| Keybind        | Description                                                  |
| -------------- | ------------------------------------------------------------ |
| `Enter`        | Evaluate input                                               |
| `Ctrl + C`     | Abort evaluation **[WIP]**                                   |
| `Ctrl + D`     | Exit the program                                             |
| `Ctrl + R`     | Open history browser                                         |
| `Tab`          | Accept ghost text/current completion or open completion menu |
| `Ctrl + Space` | Open completion menu                                         |
| `Esc`          | Close completion menu                                        |

The REPL opens an IDE-style completion popup dynamically as you type symbol characters. Inline ghost text is disabled by default; enable it with `--completion-ghost-text`. Use `Tab` or `Right Arrow` to accept ghost text when enabled, `Tab` to cycle/accept popup entries, `Shift+Tab` to move backward, and `Esc` to close the popup.

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

Disable ANSI coloring with:

```sh
wolfie --no-color
```

Enable inline completion ghost text, or disable the popup completion menu, with:

```sh
wolfie --completion-ghost-text
wolfie --no-completion-menu
```

The same defaults can be set in `config.json` under `command` with `completion-ghost-text` and `no-completion-menu`. The legacy `no-completion-ghost-text` key is still accepted for disabling a previously enabled ghost-text default.

## Commands

Lines that start with `:` are handled by the CLI instead of being evaluated as Wolfram Language input:

| Command       | Description                   |
| ------------- | ----------------------------- |
| :clear        | Clear the console             |
| :setting      | Open the friendly settings menu |
| :config       | Open the friendly settings menu |
| :config show  | Show config file location       |
| :config edit  | Open config file in `$EDITOR`   |
| :help         | Print help message              |
| :history      | Search command history          |
| :!{command}   | Run a command in your shell     |
| :theme        | Cycle current theme             |
| :theme {name} | Set theme to {name}             |
| :theme list   | List all theme names            |
| :theme show   | Show current theme              |
| :quit         | Quit the shell                  |

Command completions are available only when the line starts with `:`. Wolfram Language completions are disabled for those command lines.

Use `:!` to run an external command through your shell without leaving the REPL. The command inherits the REPL's standard input, output, and error streams:

```sh
:!ls -la
:!git status
```

While typing a `:!` command, Wolfie uses shell-oriented highlighting and offers completions for executable command names on `PATH`, plus files and directories for path-like arguments. On Windows, command discovery respects `PATHEXT`.

## Themes

Theme selections made with `:theme`, `:theme {name}`, or the `:setting` / `:config` menu are persisted to the user config file and restored the next time the REPL starts.

The settings menu can also save CLI defaults such as prompts, completion UI, colors, FrontEnd use, and WSTP link options. Explicit command-line flags override these defaults where the CLI has a value to override. To ignore the saved config and use fresh in-memory defaults for only the current session, start Wolfie with `--skip-config`:

```sh
wolfie --skip-config
```

```json
{
	"$schema": "https://raw.githubusercontent.com/ToneAr/wolfie/main/schemas/config.schema.json",
	"theme": "dark",
	"command": {
		"no-frontend": false,
		"no-color": false,
		"no-welcome": false,
		"no-prompt": false,
		"completion-ghost-text": false,
		"no-completion-ghost-text": false,
		"no-completion-menu": false,
		"linkconnect": false,
		"linkname": "my-link",
		"linkprotocol": "SharedMemory",
		"linkmode": "Listen",
		"linkoptions": 4,
		"linkinit": false
	}
}
```

The JSON schema for this file is available at [`schemas/config.schema.json`](schemas/config.schema.json).

`linkprotocol` accepts `SharedMemory`, `TCPIP`, or `IntraProcess`. `linkmode` and `linkoptions` are passed to the WSTP link when connecting to an existing kernel, and to the launched kernel command when `wolfie` starts the kernel. With `--linkconnect --linkoptions 4`, `--linkinit` initializes the connected kernel by setting its current directory to the directory where `wolfie` was launched. For config-based link connections, set `"linkinit": true`; `"linkconnect": true` alone does not enable directory initialization.

Default user paths:

| Purpose       | Unix/Linux                                                                  | Windows                             |
| ------------- | --------------------------------------------------------------------------- | ----------------------------------- |
| Settings      | `$XDG_CONFIG_HOME/wolfie/config.json` or `~/.config/wolfie/config.json`     | `%APPDATA%\\wolfie\\config.json`    |
| Custom themes | `$XDG_CONFIG_HOME/wolfie/themes/*.json` or `~/.config/wolfie/themes/*.json` | `%APPDATA%\\wolfie\\themes\\*.json` |

Custom theme files are picked up automatically at REPL startup and shown by `:theme list`. Theme names cannot contain whitespace. A custom theme may inherit from a built-in `base` theme (`dark`, `light`, `solarized`, `gruvbox`, `monokai`, or `plain`) and override any subset of style fields:

```json
{
	"name": "my-theme",
	"base": "dark",
	"styles": {
		"string": "#d7af5f",
		"comment": {
			"fg": 244,
			"italic": true
		},
		"number": {
			"fg": "bright-yellow"
		},
		"builtin_symbol": {
			"fg": "cyan",
			"bold": true
		},
		"visual_selection": {
			"fg": "white",
			"bg": "#5f0000"
		},
		"prompt_left": {
			"fg": "bright-red",
			"bold": true
		}
	}
}
```

Colors can be ANSI indexes (`208`), RGB arrays (`[255, 128, 0]`), hex strings (`"#ff8000"`), or common color names such as `red`, `cyan`, `bright-blue`, and `dark-gray`.

## Kernel Discovery

Set `WOLFRAM_KERNEL` to override the kernel executable. Without that override, the CLI asks `wolframscript -showkernels` for the best local kernel path, falls back to `wolfram-app-discovery`, and prefers the native kernel binary under `SystemFiles/Kernel/Binaries` before falling back to `WolframKernel` on `PATH`.

Set `WOLFRAM_FRONTEND` to override the FrontEnd executable used for FrontEnd-backed completions and rendering support.

## Kernel during build

The `wstp` crate links Wolfram's WSTP static library at build time. A build machine must have a Wolfram installation or WSTP SDK for the Rust target being built. If discovery does not find it, set `WSTP_COMPILER_ADDITIONS_DIRECTORY` to the target's `SystemFiles/Links/WSTP/DeveloperKit/<SystemID>/CompilerAdditions` directory.

## Release Builds

GitHub Actions builds packaged binaries when a `v*` or `build*` tag is pushed. `test*` tags and manual workflow runs exercise the build/test path without packaging or publishing artifacts, unless the manual run is explicitly started from a `v*` or `build*` tag ref.

Release builds run on GitHub-hosted runners. Because GitHub-hosted runners do not include Wolfram and `wstp-sys` links the target WSTP static library during `cargo build`, the workflow extracts the required `CompilerAdditions` from official Wolfram Engine artifacts before building:

| Artifact         | Runner           | Rust target                | WSTP source                 |
| ---------------- | ---------------- | -------------------------- | --------------------------- |
| `linux-x86_64`   | `ubuntu-latest`  | `x86_64-unknown-linux-gnu` | Wolfram Engine Docker image |
| `macos-x86_64`   | `macos-15-intel` | `x86_64-apple-darwin`      | Wolfram Engine macOS DMG    |
| `macos-aarch64`  | `macos-15`       | `aarch64-apple-darwin`     | Wolfram Engine macOS DMG    |
| `windows-x86_64` | `windows-latest` | `x86_64-pc-windows-msvc`   | Wolfram Engine Windows MSI  |

Locally, set `WSTP_COMPILER_ADDITIONS_DIRECTORY` if automatic discovery does not find the target's `SystemFiles/Links/WSTP/DeveloperKit/<SystemID>/CompilerAdditions` directory. Linux builds also need the system `uuid` library available for linking, for example the `uuid-dev` package on Debian/Ubuntu systems.

The packaged binary locates the user's Wolfram installation at runtime using the
discovery behavior above. Expression, REPL, completion, and script-file
evaluation run over WSTP.

## Regenerating build-time kernel data

Builds embed pre-generated kernel data from files committed under `build_tools/`; they do not launch `WolframKernel` during `cargo build`. When the generated data needs to be refreshed, run:

```sh
build_tools/generate-kernel-data.sh
```

Set `WOLFRAM_KERNEL=/path/to/WolframKernel` to force a specific kernel. The script currently regenerates `build_tools/builtin_symbols.tsv`; commit that file with the source change that requires the refresh.
