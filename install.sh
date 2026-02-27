#!/bin/sh
# Bear installer — downloads pre-built binaries from GitHub Releases.
# Usage: curl -fsSL https://raw.githubusercontent.com/applegrew/bear/master/install.sh | sh
set -e

REPO="applegrew/bear"
INSTALL_DIR="$HOME/.bear/bin"

# ---------------------------------------------------------------------------
# Detect OS and architecture
# ---------------------------------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS_LABEL="macos" ;;
  Linux)  OS_LABEL="linux" ;;
  *)
    echo "Error: unsupported operating system: $OS" >&2
    echo "Bear supports macOS and Linux." >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_LABEL="x86_64" ;;
  arm64|aarch64)  ARCH_LABEL="arm64" ;;
  *)
    echo "Error: unsupported architecture: $ARCH" >&2
    echo "Bear supports x86_64 and arm64." >&2
    exit 1
    ;;
esac

# Linux ARM builds are not yet available
if [ "$OS_LABEL" = "linux" ] && [ "$ARCH_LABEL" = "arm64" ]; then
  echo "Error: pre-built Linux ARM64 binaries are not yet available." >&2
  echo "Please build from source: https://github.com/$REPO#quick-start" >&2
  exit 1
fi

ARTIFACT="bear-${OS_LABEL}-${ARCH_LABEL}.tar.gz"

echo "Detected: $OS ($ARCH) -> $ARTIFACT"

# ---------------------------------------------------------------------------
# Fetch latest release tag
# ---------------------------------------------------------------------------

echo "Fetching latest release..."

if command -v curl >/dev/null 2>&1; then
  RELEASE_JSON="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest")"
elif command -v wget >/dev/null 2>&1; then
  RELEASE_JSON="$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest")"
else
  echo "Error: curl or wget is required." >&2
  exit 1
fi

# Extract tag_name without requiring jq
TAG="$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"

if [ -z "$TAG" ]; then
  echo "Error: could not determine latest release." >&2
  exit 1
fi

echo "Latest release: $TAG"

# ---------------------------------------------------------------------------
# Download and install
# ---------------------------------------------------------------------------

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$TAG/$ARTIFACT"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading $DOWNLOAD_URL ..."

if command -v curl >/dev/null 2>&1; then
  curl -fSL "$DOWNLOAD_URL" -o "$TMP_DIR/$ARTIFACT"
else
  wget -q "$DOWNLOAD_URL" -O "$TMP_DIR/$ARTIFACT"
fi

mkdir -p "$INSTALL_DIR"
tar xzf "$TMP_DIR/$ARTIFACT" -C "$INSTALL_DIR"
chmod +x "$INSTALL_DIR/bear" "$INSTALL_DIR/bear-server"

echo "Installed bear and bear-server to $INSTALL_DIR"

# ---------------------------------------------------------------------------
# Add to PATH if needed
# ---------------------------------------------------------------------------

add_to_path() {
  local profile="$1"
  if [ -f "$profile" ]; then
    if ! grep -q "$INSTALL_DIR" "$profile" 2>/dev/null; then
      echo "" >> "$profile"
      echo "# Bear" >> "$profile"
      echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$profile"
      echo "  Added $INSTALL_DIR to PATH in $profile"
    fi
  fi
}

case "$SHELL" in
  */zsh)
    add_to_path "$HOME/.zshrc"
    ;;
  */bash)
    if [ -f "$HOME/.bash_profile" ]; then
      add_to_path "$HOME/.bash_profile"
    else
      add_to_path "$HOME/.bashrc"
    fi
    ;;
  *)
    add_to_path "$HOME/.profile"
    ;;
esac

# Also check if it's already on PATH
if echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  IN_PATH=true
else
  IN_PATH=false
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

echo ""
echo "  Bear $TAG installed successfully!"
echo ""

if [ "$IN_PATH" = false ]; then
  echo "  Run this to add bear to your current shell:"
  echo ""
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
  echo ""
fi

echo "  Get started:"
echo ""
echo "    bear"
echo ""
