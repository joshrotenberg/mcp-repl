#!/bin/sh
# Install mcp-repl from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/install.sh | sh
#
# Or read it first, which is the better habit for anything piped to a shell:
#
#   curl -fsSLO https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/install.sh
#   less install.sh && sh install.sh
#
# Environment:
#   MCP_REPL_VERSION      tag to install (default: the latest release)
#   MCP_REPL_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#
# POSIX sh on purpose: this runs before anything is installed, on whatever
# the machine already has.

set -eu

REPO="joshrotenberg/mcp-repl"
RUST_MSRV=1.90.0
GNU_LIBC_FLOOR=2.34

say() { printf '%s\n' "$*"; }
die() {
    printf 'error: %s\n' "$*" >&2
    printf 'fallback: install from source with Rust %s or newer:\n' \
        "$RUST_MSRV" >&2
    printf '%s\n' '  cargo install --locked mcp-repl' >&2
    exit 1
}

need() {
    command -v "$1" > /dev/null 2>&1 || die "this script needs $1"
}

valid_version() {
    case "$1" in
        v*) value=${1#v} ;;
        *) return 1 ;;
    esac

    major=${value%%.*}
    rest=${value#*.}
    [ "$rest" != "$value" ] || return 1
    minor=${rest%%.*}
    patch=${rest#*.}
    [ "$patch" != "$rest" ] || return 1

    case "$major" in '' | *[!0-9]*) return 1 ;; esac
    case "$minor" in '' | *[!0-9]*) return 1 ;; esac
    case "$patch" in '' | *[!0-9]* | *.*) return 1 ;; esac
}

# Print a platform choice only when the probe is decisive. An explicit musl
# result always wins. A recognizable glibc is usable only at the release
# binary's glibc floor; everything else is left for the next probe or the
# final musl fallback.
classify_libc() {
    probe=$1
    lower=$(printf '%s\n' "$probe" | LC_ALL=C tr '[:upper:]' '[:lower:]')
    case "$lower" in
        *musl*) printf '%s\n' musl; return 0 ;;
        *glibc*) version_text=${lower#*glibc} ;;
        *"gnu libc"*) version_text=${lower#*"gnu libc"} ;;
        *"gnu c library"*) version_text=${lower#*"gnu c library"} ;;
        *) return 1 ;;
    esac

    # Ignore unrelated versions printed before the libc marker (for example,
    # an operating-system release). Only a dotted version following the marker
    # can prove that the GNU artifact is compatible.
    candidate=$(printf '%s\n' "$version_text" | LC_ALL=C sed -n \
        's/^[[:space:]():,-]*\([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p')
    [ -n "$candidate" ] || return 1
    floor_major=${GNU_LIBC_FLOOR%%.*}
    floor_minor=${GNU_LIBC_FLOOR#*.}
    libc_major=${candidate%%.*}
    libc_minor=${candidate#*.}

    if [ "$libc_major" -gt "$floor_major" ] 2> /dev/null; then
        printf '%s\n' gnu
    elif [ "$libc_major" -eq "$floor_major" ] 2> /dev/null &&
         [ "$libc_minor" -ge "$floor_minor" ] 2> /dev/null; then
        printf '%s\n' gnu
    else
        printf '%s\n' musl
    fi
}

detect_linux_libc() {
    if command -v getconf > /dev/null 2>&1; then
        probe=$(LC_ALL=C getconf GNU_LIBC_VERSION 2> /dev/null || :)
        if choice=$(classify_libc "$probe"); then
            printf '%s\n' "$choice"
            return
        fi
    fi

    if command -v ldd > /dev/null 2>&1; then
        probe=$(LC_ALL=C ldd --version 2>&1 || :)
        if choice=$(classify_libc "$probe"); then
            printf '%s\n' "$choice"
            return
        fi
    fi

    printf '%s\n' musl
}

need uname
need mkdir
need tar
need mktemp
need rm
need cp
need chmod
need mv
need cmp
need wc
need sed
need tr

if [ -n "${MCP_REPL_INSTALL_DIR:-}" ]; then
    INSTALL_DIR=$MCP_REPL_INSTALL_DIR
elif [ -n "${HOME:-}" ]; then
    INSTALL_DIR=$HOME/.local/bin
else
    die 'HOME is unset; set MCP_REPL_INSTALL_DIR to choose an install directory'
fi

# curl or wget, whichever is here.
if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    latest_url() {
        curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest"
    }
elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
    latest_url() {
        wget -qS --max-redirect=0 \
            "https://github.com/$REPO/releases/latest" 2>&1 \
            | sed -n 's|.*Location: *\([^ ]*\).*|\1|p' | tr -d '\r'
    }
else
    die "this script needs curl or wget"
fi

# The triples the release workflow builds. An unlisted platform is told so
# rather than handed something that will not run.
os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64 | amd64) arch_part=x86_64 ;;
    aarch64 | arm64) arch_part=aarch64 ;;
    *) die "no prebuilt binary for architecture $arch" ;;
esac
case "$os" in
    Linux)
        libc=$(detect_linux_libc)
        target="${arch_part}-unknown-linux-${libc}"
        ;;
    Darwin) target="${arch_part}-apple-darwin" ;;
    *) die "no prebuilt binary for operating system $os" ;;
esac

version=${MCP_REPL_VERSION:-}
if [ -z "$version" ]; then
    say "looking up the latest release..."
    landed=$(latest_url || :)
    release_tag_url="https://github.com/$REPO/releases/tag/"
    # Accept only GitHub's exact release-tag origin and repository path. A
    # repository with no releases lands elsewhere, and an untrusted redirect
    # must never become part of a release asset URL.
    case "$landed" in
        "$release_tag_url"*) version=${landed#"$release_tag_url"} ;;
        '') die "could not determine $REPO's latest published release" ;;
        *) die "latest release did not resolve under $release_tag_url" ;;
    esac
fi
valid_version "$version" || die "release version must match vX.Y.Z: $version"

archive="mcp-repl-${version}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${version}/${archive}"
member="mcp-repl-${version}-${target}/mcp-repl"

tmp=$(mktemp -d) || die "could not create a temporary directory"
staged=
cleanup() {
    [ -z "$staged" ] || rm -f "$staged"
    rm -rf "$tmp"
}
# Clean up whether this succeeds or not, including on Ctrl-C.
trap cleanup 0
trap 'exit 1' 1 2 15

say "downloading $archive"
fetch "$url" "$tmp/$archive" \
    || die "no such release asset: $url"

# Verify before unpacking. A checksum that cannot be fetched is a failure
# rather than a warning: silently skipping the check is how an install
# script becomes the weak link.
say "verifying checksum"
fetch "$url.sha256" "$tmp/$archive.sha256" \
    || die "no checksum published for $archive"

checksum_lines=$(wc -l < "$tmp/$archive.sha256" | tr -d '[:space:]')
[ "$checksum_lines" = 1 ] || die "checksum for $archive is not one canonical line"
IFS= read -r checksum_line < "$tmp/$archive.sha256" \
    || die "checksum for $archive is not newline-terminated"
expected=${checksum_line%% *}
case "$expected" in *[!0-9a-f]*) die "checksum for $archive is not lowercase SHA-256" ;; esac
[ "${#expected}" -eq 64 ] || die "checksum for $archive is not lowercase SHA-256"
expected_checksum=$tmp/expected-checksum
printf '%s  %s\n' "$expected" "$archive" > "$expected_checksum"
cmp -s "$tmp/$archive.sha256" "$expected_checksum" \
    || die "checksum does not identify $archive exactly"

if command -v sha256sum > /dev/null 2>&1; then
    digest_output=$(sha256sum "$tmp/$archive") \
        || die "could not compute the SHA-256 checksum for $archive"
elif command -v shasum > /dev/null 2>&1; then
    digest_output=$(shasum -a 256 "$tmp/$archive") \
        || die "could not compute the SHA-256 checksum for $archive"
else
    die "this script needs sha256sum or shasum to verify the download"
fi
actual=${digest_output%% *}
case "$actual" in *[!0-9a-f]*) die "checksum tool returned a non-canonical SHA-256 digest" ;; esac
[ "${#actual}" -eq 64 ] || die "checksum tool returned a non-canonical SHA-256 digest"
[ "$expected" = "$actual" ] \
    || die "checksum mismatch: expected $expected, got $actual"

# Extract only the expected member into a fresh directory. Do not let archive
# contents choose paths on the host, and reject a symlink in place of the
# executable before copying anything into the install directory.
extract_dir=$tmp/extract
mkdir "$extract_dir" || die "could not create the extraction directory"
member_names=$(tar tzf "$tmp/$archive" "$member") \
    || die "release archive does not contain $member safely"
[ "$member_names" = "$member" ] \
    || die "release archive must contain $member exactly once"
member_details=$(LC_ALL=C tar tvzf "$tmp/$archive" "$member") \
    || die "could not inspect release archive member $member"
case "$member_details" in
    -*) ;;
    *) die "release archive member $member is not a regular file" ;;
esac
tar xzf "$tmp/$archive" -C "$extract_dir" "$member" \
    || die "release archive does not contain $member safely"
candidate=$extract_dir/$member
[ -f "$candidate" ] && [ ! -L "$candidate" ] \
    || die "release archive member $member is not a regular file"

mkdir -p "$INSTALL_DIR" || die "could not create install directory $INSTALL_DIR"
destination=$INSTALL_DIR/mcp-repl
[ ! -d "$destination" ] || die "install destination is a directory: $destination"
staged=$(mktemp "$INSTALL_DIR/.mcp-repl.XXXXXX") \
    || die "could not create a staged file in $INSTALL_DIR"
cp "$candidate" "$staged" || die "could not stage mcp-repl in $INSTALL_DIR"
chmod 755 "$staged" || die "could not make the staged mcp-repl executable"

version_output=$tmp/staged-version
if ! "$staged" --version > "$version_output" 2> /dev/null; then
    die "the staged mcp-repl binary did not run on this host"
fi
expected_version="mcp-repl ${version#v}"
expected_version_output=$tmp/expected-version
printf '%s\n' "$expected_version" > "$expected_version_output"
cmp -s "$version_output" "$expected_version_output" \
    || die "the staged binary did not report exactly '$expected_version'"

# mktemp put the staged file on the destination filesystem, so this rename is
# the only operation that replaces an existing installation and is atomic.
mv -f "$staged" "$destination" || die "could not atomically replace $destination"
staged=

say "installed mcp-repl $version to $destination"

# Say plainly whether this is usable yet, rather than leaving a "command not
# found" to be worked out later.
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        say "run \`mcp-repl --demo\` to try it"
        ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH. Add it:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

say ""
say "the archive also has shell completions and a man page, and the binary"
say "regenerates them: \`mcp-repl --completions zsh\`, \`mcp-repl --man\`"
