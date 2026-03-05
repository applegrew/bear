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

TAG=""

# Try the Bear portal first (avoids GitHub API rate limits)
if command -v curl >/dev/null 2>&1; then
  VERSION_JSON="$(curl -fsSL "https://bear.applegrew.com/api/version" 2>/dev/null || echo "")"
elif command -v wget >/dev/null 2>&1; then
  VERSION_JSON="$(wget -qO- "https://bear.applegrew.com/api/version" 2>/dev/null || echo "")"
else
  echo "Error: curl or wget is required." >&2
  exit 1
fi

if [ -n "$VERSION_JSON" ]; then
  TAG="$(echo "$VERSION_JSON" | grep '"version"' | head -1 | sed 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
fi

# Fallback to GitHub API if portal didn't return a version
if [ -z "$TAG" ]; then
  echo "  Portal unavailable, falling back to GitHub API..."
  if command -v curl >/dev/null 2>&1; then
    RELEASE_JSON="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null || echo "")"
  else
    RELEASE_JSON="$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null || echo "")"
  fi
  TAG="$(echo "$RELEASE_JSON" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"
fi

if [ -z "$TAG" ]; then
  echo "Error: could not determine latest release." >&2
  echo "  GitHub API may be rate-limited. Try again later or set GITHUB_TOKEN." >&2
  exit 1
fi

echo "Latest release: $TAG"

# ---------------------------------------------------------------------------
# Check if already installed and up-to-date
# ---------------------------------------------------------------------------

VERSION_FILE="$HOME/.bear/.version"
INSTALLED_VERSION=""

if [ -f "$VERSION_FILE" ]; then
  INSTALLED_VERSION="$(cat "$VERSION_FILE" 2>/dev/null || echo "")"
fi

if [ -n "$INSTALLED_VERSION" ] && [ "$INSTALLED_VERSION" = "$TAG" ]; then
  echo ""
  echo "  Bear $TAG is already installed and up-to-date."
  echo ""
  exit 0
fi

if [ -n "$INSTALLED_VERSION" ]; then
  echo "Updating from $INSTALLED_VERSION to $TAG..."
else
  echo "Installing Bear $TAG..."
fi

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

# Store installed version
mkdir -p "$(dirname "$VERSION_FILE")"
echo "$TAG" > "$VERSION_FILE"

if [ -n "$INSTALLED_VERSION" ]; then
  echo "Updated bear and bear-server to $TAG"
else
  echo "Installed bear and bear-server to $INSTALL_DIR"
fi

# ---------------------------------------------------------------------------
# Add to PATH if needed
# ---------------------------------------------------------------------------

# Check if it's already on PATH
if echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  IN_PATH=true
else
  IN_PATH=false
fi

# If not in PATH, offer to add it to shell profile
if [ "$IN_PATH" = false ]; then
  # Determine which profile to modify
  PROFILE=""
  case "$SHELL" in
    */zsh)
      PROFILE="$HOME/.zshrc"
      ;;
    */bash)
      if [ -f "$HOME/.bash_profile" ]; then
        PROFILE="$HOME/.bash_profile"
      else
        PROFILE="$HOME/.bashrc"
      fi
      ;;
    *)
      PROFILE="$HOME/.profile"
      ;;
  esac

  # Check if profile already has the PATH entry
  ALREADY_IN_PROFILE=false
  if [ -f "$PROFILE" ] && grep -q "$INSTALL_DIR" "$PROFILE" 2>/dev/null; then
    ALREADY_IN_PROFILE=true
  fi

  # Prompt user if not already in profile
  if [ "$ALREADY_IN_PROFILE" = false ]; then
    if [ -t 0 ]; then
      # stdin is a terminal — read directly
      printf "Add %s to PATH in %s? [Y/n] " "$INSTALL_DIR" "$PROFILE"
      read -r response
    elif [ -e /dev/tty ]; then
      # stdin is a pipe (e.g. curl | sh) — read from tty
      printf "Add %s to PATH in %s? [Y/n] " "$INSTALL_DIR" "$PROFILE"
      read -r response < /dev/tty
    else
      # no tty available (e.g. CI) — skip
      response="n"
    fi
    case "$response" in
      [nN][oO]|[nN])
        echo "  Skipped adding to PATH."
        ;;
      *)
        echo "" >> "$PROFILE"
        echo "# Bear" >> "$PROFILE"
        echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$PROFILE"
        echo "  Added $INSTALL_DIR to PATH in $PROFILE"
        echo "  Restart your shell or run 'source $PROFILE' for this to take effect."
        ;;
    esac
  fi
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
