#!/usr/bin/env sh
set -eu

BINARY_NAME="wolfish"
GITHUB_REPO="${GITHUB_REPO:-ToneAr/wolfish}"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-}"
BUILD_FROM_SOURCE=0
FORCE=0

usage() {
    cat <<EOF
Install wolfish on Linux or macOS.

Usage:
  ./install.sh [options]

Options:
  --install-dir DIR    Install the binary into DIR.
                       Defaults to \$HOME/.local/bin, or /usr/local/bin when
                       it is writable and already on PATH.
  --version TAG        Install a specific GitHub release tag, such as v0.2.0.
                       Defaults to the latest release.
  --build-from-source  Build this checkout with cargo and install the result.
  --force              Replace an existing binary at the destination.
  -h, --help           Show this help.

Environment:
  INSTALL_DIR          Same as --install-dir.
  VERSION              Same as --version.
  GITHUB_REPO          GitHub repo to download from. Defaults to ToneAr/wolfish.
  WOLFRAM_CLI_SHA256   Optional expected SHA-256 checksum for the release archive.
EOF
}

log() {
    printf '%s\n' "$*"
}

fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

has_command() {
    command -v "$1" >/dev/null 2>&1
}

need_command() {
    has_command "$1" || fail "required command not found: $1"
}

path_contains() {
    case ":${PATH:-}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

default_install_dir() {
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ] && path_contains /usr/local/bin; then
        printf '%s\n' "/usr/local/bin"
    else
        printf '%s\n' "$HOME/.local/bin"
    fi
}

target_name() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux) os_part="linux" ;;
        Darwin) os_part="macos" ;;
        *) fail "unsupported operating system: $os" ;;
    esac

    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *) fail "unsupported CPU architecture: $arch" ;;
    esac

    printf '%s-%s\n' "$os_part" "$arch_part"
}

download_file() {
    url="$1"
    output="$2"

    if has_command curl; then
        curl --fail --location --show-error --silent "$url" --output "$output"
    elif has_command wget; then
        wget -q "$url" -O "$output"
    else
        fail "curl or wget is required to download release archives"
    fi
}

verify_sha256() {
    file="$1"
    expected="$2"

    [ -n "$expected" ] || return 0

    if has_command sha256sum; then
        actual="$(sha256sum "$file" | awk '{print $1}')"
    elif has_command shasum; then
        actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    else
        fail "WOLFRAM_CLI_SHA256 was set, but sha256sum or shasum is required"
    fi

    [ "$actual" = "$expected" ] || fail "checksum mismatch for downloaded archive"
}

install_binary() {
    source_path="$1"
    destination_path="$2"

    [ -f "$source_path" ] || fail "binary not found at $source_path"

    if { [ -f "$destination_path" ] || [ -d "$destination_path" ] || [ -L "$destination_path" ]; } && [ "$FORCE" -ne 1 ]; then
        fail "$destination_path already exists; rerun with --force to replace it"
    fi

    mkdir -p "$INSTALL_DIR"
    tmp_destination="${destination_path}.tmp.$$"

    if has_command install; then
        install -m 0755 "$source_path" "$tmp_destination"
    else
        cp "$source_path" "$tmp_destination"
        chmod 0755 "$tmp_destination"
    fi

    mv "$tmp_destination" "$destination_path"
}

download_and_install_release() {
    need_command uname
    need_command tar
    need_command mktemp
    need_command awk

    target="$(target_name)"
    package="${BINARY_NAME}-${target}"
    archive="${package}.tar.gz"

    if [ "$VERSION" = "latest" ]; then
        url="https://github.com/${GITHUB_REPO}/releases/latest/download/${archive}"
    else
        url="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}/${archive}"
    fi

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

    log "Downloading $url"
    download_file "$url" "$tmpdir/$archive"
    verify_sha256 "$tmpdir/$archive" "${WOLFRAM_CLI_SHA256:-}"

    tar -xzf "$tmpdir/$archive" -C "$tmpdir"
    binary_path="$tmpdir/$package/$BINARY_NAME"

    if [ ! -f "$binary_path" ]; then
        binary_path="$(find "$tmpdir" -type f -name "$BINARY_NAME" | head -n 1 || true)"
    fi

    [ -n "$binary_path" ] || fail "archive did not contain $BINARY_NAME"
    install_binary "$binary_path" "$INSTALL_DIR/$BINARY_NAME"
}

build_and_install_source() {
    need_command cargo

    script_dir=$(CDPATH= cd "$(dirname "$0")" && pwd -P)
    [ -f "$script_dir/Cargo.toml" ] || fail "--build-from-source must be run from a source checkout"

    log "Building $BINARY_NAME from source"
    (
        cd "$script_dir"
        WOLFRAM_KERNEL="${WOLFRAM_KERNEL:-/nonexistent/WolframKernel}" cargo build --release --locked
    )

    install_binary "$script_dir/target/release/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --install-dir)
            [ "$#" -ge 2 ] || fail "--install-dir requires a value"
            INSTALL_DIR="$2"
            shift 2
            ;;
        --install-dir=*)
            INSTALL_DIR="${1#*=}"
            shift
            ;;
        --version)
            [ "$#" -ge 2 ] || fail "--version requires a release tag"
            VERSION="$2"
            shift 2
            ;;
        --version=*)
            VERSION="${1#*=}"
            shift
            ;;
        --build-from-source)
            BUILD_FROM_SOURCE=1
            shift
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[ -n "${HOME:-}" ] || fail "HOME is not set"

if [ -z "$INSTALL_DIR" ]; then
    INSTALL_DIR="$(default_install_dir)"
fi

case "$INSTALL_DIR" in
    /*) ;;
    *) fail "--install-dir must be an absolute path" ;;
esac

destination="$INSTALL_DIR/$BINARY_NAME"

if [ "$BUILD_FROM_SOURCE" -eq 1 ]; then
    build_and_install_source
else
    download_and_install_release
fi

log "Installed $BINARY_NAME to $destination"

if ! path_contains "$INSTALL_DIR"; then
    log "Add $INSTALL_DIR to PATH to run $BINARY_NAME without a full path."
fi
