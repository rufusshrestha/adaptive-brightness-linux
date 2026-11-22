#!/bin/bash
# Installer for LBrightness (Rust)
#
# Installs everything:
#   user:  ~/.local/bin/lbright, ~/.config/systemd/user/lbright.service (enabled + started)
#   root:  ambient light sensor reset helper (needs sudo):
#            /usr/local/libexec/lbright/als-reset
#            /etc/systemd/system/lbright-als-reset.service
#            /etc/polkit-1/rules.d/50-lbright.rules
#
# Options:
#   --user-only   Skip the root helper (no sudo). Stuck-sensor auto-reset is then unavailable.
set -e

USER_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --user-only) USER_ONLY=1 ;;
        -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $arg"; echo "Usage: $0 [--user-only]"; exit 1 ;;
    esac
done

cd "$(dirname "$0")"

echo "=== Installing LBrightness (lbright) ==="

if dpkg -s lbright >/dev/null 2>&1; then
    echo "Error: the lbright .deb package is already installed."
    echo "Remove it first (sudo apt remove lbright) or keep using the package; mixing both"
    echo "installs leaves two binaries and two service units."
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "Error: 'cargo' is required to compile lbright."
    exit 1
fi

if ! command -v monitor-sensor >/dev/null 2>&1; then
    echo "Note: 'monitor-sensor' (package iio-sensor-proxy) not found. lbright will read the"
    echo "      light sensor from sysfs only. Install iio-sensor-proxy for more reliable readings."
fi

echo "Building release binary with Cargo..."
cargo build --release

# Ensure target directories exist
mkdir -p "$HOME/.local/bin"
mkdir -p "$HOME/.config/systemd/user"
mkdir -p "$HOME/.config/lbrightness"
mkdir -p "$HOME/.local/state/lbrightness"

# Stop old python daemon if running
systemctl --user stop auto-brightness.service 2>/dev/null || true
systemctl --user disable auto-brightness.service 2>/dev/null || true

# Root helper first, so the daemon can use it as soon as it starts.
if [ "$USER_ONLY" -eq 0 ]; then
    echo "Installing ambient light sensor reset helper (requires sudo)..."
    sudo install -Dm755 packaging/als-reset /usr/local/libexec/lbright/als-reset
    sed 's|/usr/libexec/lbright/als-reset|/usr/local/libexec/lbright/als-reset|' packaging/lbright-als-reset.service \
        | sudo install -Dm644 /dev/stdin /etc/systemd/system/lbright-als-reset.service
    # Keep the polkit rule LAST: uninstall.sh relies on "no unit/script => no rule",
    # because /etc/polkit-1/rules.d isn't readable without root.
    sudo install -Dm644 packaging/50-lbright.rules /etc/polkit-1/rules.d/50-lbright.rules
    sudo systemctl daemon-reload
fi

# Install binary and user unit
install -Dm755 target/release/lbright "$HOME/.local/bin/lbright"
install -Dm644 lbright.service "$HOME/.config/systemd/user/lbright.service"

# Reload systemd and (re)start lbright so an upgrade picks up the new binary
systemctl --user daemon-reload
systemctl --user enable lbright.service
systemctl --user restart lbright.service

# The daemon writes a default config.ini on startup if missing (an existing one is kept).
CONFIG="$HOME/.config/lbrightness/config.ini"
for _ in $(seq 50); do
    [ -s "$CONFIG" ] && break
    sleep 0.1
done
if [ ! -s "$CONFIG" ]; then
    # Service didn't start (e.g. no user session bus): create it directly.
    LBRIGHT_NO_SENSOR_PROXY=1 "$HOME/.local/bin/lbright" status >/dev/null 2>&1 || true
fi
if [ -s "$CONFIG" ]; then
    echo "Configuration: $CONFIG"
else
    echo "Warning: could not create $CONFIG (it will be created when lbright first runs)."
fi

echo ""
echo "Installation complete!"
echo "Binary installed to: $HOME/.local/bin/lbright"
if [ "$USER_ONLY" -eq 0 ]; then
    echo "Sensor reset helper: lbright-als-reset.service (manual trigger: lbright als-reset)"
fi
echo ""
echo "To check system status:       lbright status"
echo "To open interactive menu:     lbright tui"
echo "To scan displays:             lbright scan"
echo "To migrate old configuration: lbright migrate --dry-run"
echo "                              lbright migrate"
echo "To uninstall:                 ./uninstall.sh"
