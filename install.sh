#!/bin/sh
# ScreenSense installer for Linux (x86_64).
#
# Downloads the latest published release for this supported target. If no
# compatible release is published yet, use the source-build instructions in
# README.md.
#
# What it does:
#   1. downloads the latest release archive and SHA256SUMS from GitHub;
#   2. verifies the archive against SHA256SUMS and stops on any mismatch;
#   3. copies the `screensense` binary to ~/.local/bin.
#
# What it does NOT do: start the daemon, edit any AI-tool configuration, or
# enable autostart. Those are separate commands you run yourself.

set -eu

REPO="stevenke1981/screensense"
ASSET="screensense-x86_64-unknown-linux-gnu.tar.gz"
BASE="https://github.com/$REPO/releases/latest/download"
DEST="${HOME}/.local/bin"

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "this script is for Linux; on Windows use install.ps1"
case "$(uname -m)" in
  x86_64 | amd64) ;;
  *) die "no prebuilt binary for $(uname -m); build from source (see README.md)" ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "sha256sum or shasum is required to verify the download"
fi

command -v tar >/dev/null 2>&1 || die "tar is required"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

say "Downloading $ASSET ..."
fetch "$BASE/$ASSET" "$TMP/$ASSET" ||
  die "download failed: $BASE/$ASSET (has a release been published yet?)"
fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" ||
  die "download failed: $BASE/SHA256SUMS"

# Lines look like "<64 hex>  <file name>" (a "*" may precede the name).
want="$(awk -v f="$ASSET" '{ n = $2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' "$TMP/SHA256SUMS")"
[ -n "$want" ] || die "SHA256SUMS has no entry for $ASSET; nothing was installed"
got="$(sha256 "$TMP/$ASSET")"
if [ "$want" != "$got" ]; then
  die "checksum mismatch for $ASSET (expected $want, got $got); nothing was installed"
fi
say "Checksum OK."

mkdir "$TMP/unpacked"
tar -xzf "$TMP/$ASSET" -C "$TMP/unpacked"
[ -f "$TMP/unpacked/screensense" ] || die "the archive does not contain a screensense binary"

mkdir -p "$DEST"
# Copy next to the target, then rename: replacing a running binary in place fails.
cp "$TMP/unpacked/screensense" "$DEST/screensense.new"
chmod 755 "$DEST/screensense.new"
mv -f "$DEST/screensense.new" "$DEST/screensense"
say "Installed $DEST/screensense"

case ":${PATH}:" in
  *":$DEST:"*) ;;
  *)
    say ""
    say "WARNING: $DEST is not on your PATH. Add this to your shell's rc file:"
    say "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
esac

if ! command -v tesseract >/dev/null 2>&1; then
  say ""
  say "NOTE: tesseract was not found. Text recognition needs it, for example:"
  say "  sudo apt install tesseract-ocr"
fi

say ""
say "Nothing has been started or configured. Next steps:"
say "  screensense doctor --capture   # check what works on this machine"
say "  screensense daemon start       # start recording"
say "  screensense setup              # connect your AI coding tools"
say "If a daemon from an older version is running: screensense daemon restart"
