#!/usr/bin/env sh
set -eu

BINARY_NAME="wolfie"
GITHUB_REPO="${GITHUB_REPO:-ToneAr/wolfie}"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-}"
CONFIG_SCHEMA_URL="https://raw.githubusercontent.com/ToneAr/wolfie/main/schemas/config.schema.json"
BUILD_FROM_SOURCE=0
FORCE=0

usage() {
    cat <<EOF
Install wolfie on Linux or macOS.

Usage:
  ./install.sh [options]

Options:
  --install-dir DIR    Install the binary into DIR.
                       Defaults to \$HOME/.local/bin, or /usr/local/bin when
                       it is writable and already on PATH.
  --version TAG        Install a specific GitHub release tag, such as v0.2.0.
                       Defaults to the latest release.
                       The install directory is added to user PATH automatically.
  --build-from-source  Build this checkout with cargo and install the result.
  --force              Replace an existing binary at the destination.
  -h, --help           Show this help.

Environment:
  INSTALL_DIR          Same as --install-dir.
  VERSION              Same as --version.
  GITHUB_REPO          GitHub repo to download from. Defaults to ToneAr/wolfie.
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

shell_quote() {
    printf "'"
    printf '%s' "$1" | sed "s/'/'\\''/g"
    printf "'"
}

path_profile_file() {
    shell_name="$(basename "${SHELL:-}")"
    os_name="$(uname -s 2>/dev/null || printf '%s' '')"

    if [ "$shell_name" = "zsh" ] || { [ "$os_name" = "Darwin" ] && [ -z "$shell_name" ]; }; then
        printf '%s\n' "$HOME/.zprofile"
    elif [ "$shell_name" = "bash" ]; then
        printf '%s\n' "$HOME/.bashrc"
    else
        printf '%s\n' "$HOME/.profile"
    fi
}

path_profile_block() {
    quoted_install_dir="$(shell_quote "$INSTALL_DIR")"
    cat <<EOF
# >>> wolfie installer >>>
# Add wolfie to PATH.
wolfie_install_dir=$quoted_install_dir
case ":\$PATH:" in
    *":\$wolfie_install_dir:"*) ;;
    *) export PATH="\$wolfie_install_dir:\$PATH" ;;
esac
unset wolfie_install_dir
# <<< wolfie installer <<<
EOF
}

remove_existing_path_block() {
    profile_file="$1"
    tmp_file="${profile_file}.tmp.$$"

    [ -f "$profile_file" ] || return 0

    awk '
        /^# >>> wolfie installer >>>$/ {
            in_block = 1
            next
        }
        /^# <<< wolfie installer <<<$/ {
            if (in_block) {
                in_block = 0
                next
            }
        }
        !in_block {
            print
        }
    ' "$profile_file" > "$tmp_file"
    mv "$tmp_file" "$profile_file"
}

add_install_dir_to_path() {
    profile_file="$(path_profile_file)"

    if ! path_contains "$INSTALL_DIR"; then
        if [ -n "${PATH:-}" ]; then
            export PATH="$INSTALL_DIR:$PATH"
        else
            export PATH="$INSTALL_DIR"
        fi
    fi

    mkdir -p "$(dirname "$profile_file")"
    touch "$profile_file"
    remove_existing_path_block "$profile_file"

    {
        if [ -s "$profile_file" ]; then
            printf '\n'
        fi
        path_profile_block
    } >> "$profile_file"

    log "Added $INSTALL_DIR to PATH in $profile_file."
    log "Open a new terminal for the PATH change to be available everywhere."
}

default_install_dir() {
    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ] && path_contains /usr/local/bin; then
        printf '%s\n' "/usr/local/bin"
    else
        printf '%s\n' "$HOME/.local/bin"
    fi
}

default_config_dir() {
    if [ -n "${XDG_CONFIG_HOME:-}" ]; then
        printf '%s\n' "$XDG_CONFIG_HOME/wolfie"
    else
        printf '%s\n' "$HOME/.config/wolfie"
    fi
}

create_default_config() {
    config_dir="$(default_config_dir)"
    config_file="$config_dir/config.json"

    [ -e "$config_file" ] && return 0

    mkdir -p "$config_dir"
    cat > "$config_file" <<EOF
{
  "\$schema": "$CONFIG_SCHEMA_URL"
}
EOF
    log "Created default config at $config_file"
}

glibc_version() {
    if has_command getconf; then
        getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}'
    elif has_command ldd; then
        ldd --version 2>/dev/null | awk 'NR == 1 {print $NF}'
    else
        printf '%s\n' ""
    fi
}

version_ge() {
    left_major="${1%%.*}"
    left_rest="${1#*.}"
    right_major="${2%%.*}"
    right_rest="${2#*.}"
    left_minor="${left_rest%%.*}"
    right_minor="${right_rest%%.*}"

    [ -n "$left_major" ] || return 1
    [ -n "$left_minor" ] || left_minor=0

    if [ "$left_major" -gt "$right_major" ]; then
        return 0
    fi
    if [ "$left_major" -eq "$right_major" ] && [ "$left_minor" -ge "$right_minor" ]; then
        return 0
    fi
    return 1
}

os_release_value() {
    key="$1"
    [ -f /etc/os-release ] || return 0
    awk -F= -v key="$key" '
        $1 == key {
            value = $2
            gsub(/^"|"$/, "", value)
            print value
            exit
        }
    ' /etc/os-release
}

linux_target_name() {
    arch_part="$1"

    [ "$arch_part" = "x86_64" ] || fail "unsupported CPU architecture: Linux releases require x86_64"

    os_id="$(os_release_value ID)"
    os_like="$(os_release_value ID_LIKE)"
    version_id="$(os_release_value VERSION_ID)"
    distro_tags=" $os_id $os_like "
    distro_major="${version_id%%.*}"

    case "$distro_tags" in
        *" rhel "* | *" fedora "* | *" centos "*)
            case "$distro_major" in
                8) printf '%s\n' "linux-rhel8-x86_64"; return ;;
                9) printf '%s\n' "linux-rhel9-x86_64"; return ;;
            esac
            ;;
    esac

    glibc="$(glibc_version)"
    if [ -n "$glibc" ] && version_ge "$glibc" "2.39"; then
        printf '%s\n' "linux-ubuntu24-x86_64"
    elif [ -n "$glibc" ] && version_ge "$glibc" "2.34"; then
        printf '%s\n' "linux-rhel9-x86_64"
    else
        printf '%s\n' "linux-rhel8-x86_64"
    fi
}

target_name() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *) fail "unsupported CPU architecture: $arch" ;;
    esac

    case "$os" in
        Linux) linux_target_name "$arch_part" ;;
        Darwin) printf '%s-%s\n' "macos" "$arch_part" ;;
        *) fail "unsupported operating system: $os" ;;
    esac
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
create_default_config

if ! path_contains "$INSTALL_DIR"; then
    add_install_dir_to_path
fi
