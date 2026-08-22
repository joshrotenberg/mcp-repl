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
INSTALL_DIR="${MCP_REPL_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" > /dev/null 2>&1 || die "this script needs $1"
}

need uname
need mkdir
need tar

# curl or wget, whichever is here.
if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
else
    die "this script needs curl or wget"
fi

# The triples the release workflow builds. An unlisted platform is told so
# rather than handed something that will not run.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)  os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *) die "no prebuilt binary for $os; with Rust 1.90+, install with \`cargo install --locked mcp-repl\`" ;;
esac
case "$arch" in
    x86_64 | amd64)  arch_part="x86_64" ;;
    aarch64 | arm64) arch_part="aarch64" ;;
    *) die "no prebuilt binary for $arch; with Rust 1.90+, install with \`cargo install --locked mcp-repl\`" ;;
esac
target="${arch_part}-${os_part}"

version="${MCP_REPL_VERSION:-}"
if [ -z "$version" ]; then
    say "looking up the latest release..."
    # The redirect from /releases/latest names the tag, which avoids needing
    # a JSON parser on a machine that may not have one.
    if command -v curl > /dev/null 2>&1; then
        landed="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/$REPO/releases/latest" || true)"
    else
        landed="$(wget -qS --max-redirect=0 \
            "https://github.com/$REPO/releases/latest" 2>&1 \
            | sed -n 's|.*Location: *\([^ ]*\).*|\1|p' | tr -d '\r')"
    fi
    # A repository with no releases redirects to /releases, with no tag in
    # it. Without this check the URL itself became the "version" and the
    # download failed with a nonsense name.
    case "$landed" in
        */tag/*) version="${landed##*/tag/}" ;;
        *) die "$REPO has no published release yet; with Rust 1.90+, install with \`cargo install --locked mcp-repl\`" ;;
    esac
    [ -n "$version" ] || die "could not determine the latest release"
fi

archive="mcp-repl-${version}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/${version}/${archive}"

tmp="$(mktemp -d)"
# Clean up whether this succeeds or not, including on Ctrl-C.
trap 'rm -rf "$tmp"' EXIT INT TERM

say "downloading $archive"
fetch "$url" "$tmp/$archive" \
    || die "no such release asset: $url"

# Verify before unpacking. A checksum that cannot be fetched is a failure
# rather than a warning: silently skipping the check is how an install
# script becomes the weak link.
say "verifying checksum"
fetch "$url.sha256" "$tmp/$archive.sha256" \
    || die "no checksum published for $archive"

expected="$(awk '{print $1}' "$tmp/$archive.sha256")"
if command -v sha256sum > /dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
elif command -v shasum > /dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
else
    die "this script needs sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected, got $actual"

tar xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/mcp-repl-${version}-${target}/mcp-repl" "$INSTALL_DIR/mcp-repl" \
    2> /dev/null \
    || { cp "$tmp/mcp-repl-${version}-${target}/mcp-repl" "$INSTALL_DIR/mcp-repl" \
         && chmod 755 "$INSTALL_DIR/mcp-repl"; }

say "installed mcp-repl $version to $INSTALL_DIR/mcp-repl"

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
