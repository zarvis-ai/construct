#!/bin/sh
# construct installer.
#
#   curl -fsSL https://raw.githubusercontent.com/zarvis-ai/agentd/main/install.sh | sh
#
# Downloads the prebuilt binaries for your platform from a GitHub Release,
# verifies their SHA-256 checksum, and drops them all into one directory on
# your PATH. The daemon finds its adapters as siblings of its own binary, so
# everything must live together — this script keeps them together.
#
# Environment overrides:
#   CONSTRUCT_VERSION   release tag to install (e.g. v0.2.0). Default: latest.
#   CONSTRUCT_BIN_DIR   install directory.       Default: $HOME/.local/bin.
#   CONSTRUCT_BASE_URL  download base URL (mirror / testing). Default: the
#                       GitHub release for CONSTRUCT_VERSION. The script fetches
#                       <base>/constructd-<target>.tar.gz and <base>/SHA256SUMS.
set -eu

REPO="zarvis-ai/agentd"
VERSION="${CONSTRUCT_VERSION:-latest}"
BIN_DIR="${CONSTRUCT_BIN_DIR:-$HOME/.local/bin}"
BINS="construct construct-mcp construct-adapter-shell construct-adapter-claude construct-adapter-codex construct-adapter-antigravity construct-adapter-smith"

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- detect platform ------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) err "unsupported macOS architecture: $arch" ;;
    esac ;;
  Linux)
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
      *) err "unsupported Linux architecture: $arch" ;;
    esac ;;
  *) err "unsupported OS: $os (only macOS and Linux have prebuilt binaries; build from source instead)" ;;
esac

# --- pick a downloader + sha tool ----------------------------------------
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  err "need curl or wget to download"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  err "need sha256sum or shasum to verify the download"
fi

# --- resolve URLs ---------------------------------------------------------
asset="constructd-${target}.tar.gz"
if [ -n "${CONSTRUCT_BASE_URL:-}" ]; then
  base="${CONSTRUCT_BASE_URL%/}"
elif [ "$VERSION" = "latest" ]; then
  base="https://github.com/${REPO}/releases/latest/download"
else
  base="https://github.com/${REPO}/releases/download/${VERSION}"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Installing construct ($VERSION) for $target"

if ! fetch "${base}/${asset}" "${tmp}/${asset}"; then
  err "could not download ${base}/${asset}
     The release may not exist yet, or the repository is still private
     (no public download URL). Check https://github.com/${REPO}/releases"
fi
fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS" || err "could not download SHA256SUMS"

# --- verify checksum ------------------------------------------------------
want="$(grep " ${asset}$" "${tmp}/SHA256SUMS" | awk '{print $1}')"
[ -n "$want" ] || err "no checksum for ${asset} in SHA256SUMS"
got="$(sha256 "${tmp}/${asset}")"
[ "$want" = "$got" ] || err "checksum mismatch for ${asset}
     expected $want
     got      $got"
say "Checksum OK"

# --- install --------------------------------------------------------------
tar -xzf "${tmp}/${asset}" -C "$tmp"
src="${tmp}/constructd-${target}"
[ -d "$src" ] || err "unexpected archive layout (no constructd-${target}/ inside ${asset})"

mkdir -p "$BIN_DIR"
# Validate the whole set before touching anything (all-or-nothing-ish).
for b in $BINS; do
  [ -f "${src}/${b}" ] || err "binary '$b' missing from archive"
done
for b in $BINS; do
  # Atomic install: stage the binary as a temp file *in the destination
  # dir* (so the rename stays on one filesystem), make it executable, then
  # rename it over any existing copy. Replacing the directory entry rather
  # than writing the busy inode avoids ETXTBSY when upgrading a running
  # daemon/adapter on Linux, and matches the atomic-rename upgrade the
  # daemon's restart path expects.
  tmpbin="${BIN_DIR}/.${b}.tmp.$$"
  cp "${src}/${b}" "$tmpbin"
  chmod 0755 "$tmpbin"
  mv -f "$tmpbin" "${BIN_DIR}/${b}"
done

say "Installed into ${BIN_DIR}:"
for b in $BINS; do say "  ${b}"; done

# --- PATH guidance --------------------------------------------------------
case ":${PATH}:" in
  *":${BIN_DIR}:"*) ;;
  *)
    say ""
    say "⚠  ${BIN_DIR} is not on your PATH. Add it, e.g.:"
    say "    echo 'export PATH=\"${BIN_DIR}:\$PATH\"' >> ~/.profile   # or ~/.zshrc, ~/.bashrc"
    ;;
esac

say ""
say "Done. Try:  construct daemon run    (in another terminal)  construct"
