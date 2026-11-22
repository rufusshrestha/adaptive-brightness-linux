#!/bin/bash
# Debian Package Builder for LBrightness (lbright)
# Maintained by LeanBitLab

set -e
# Package files/dirs must not be group-writable regardless of the builder's umask
umask 022

echo "=== Building LBrightness Debian Package ==="

# Check requirements
if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "Error: 'dpkg-deb' is required to build Debian packages."
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "Error: 'cargo' is required to compile lbright."
    exit 1
fi

# Extract version from Cargo.toml
VERSION=$(grep '^version' Cargo.toml | head -n 1 | cut -d '"' -f 2)
if [ -z "$VERSION" ]; then
    VERSION="0.1.0"
fi

# Detect system architecture
ARCH=$(dpkg --print-architecture 2>/dev/null || echo "amd64")

echo "Package Version: $VERSION"
echo "Architecture:    $ARCH"

# Build Rust release binary
echo "Compiling release binary..."
cargo build --release

# Define directories
STAGE_DIR="dist/deb_build"
DEBIAN_DIR="$STAGE_DIR/DEBIAN"
BIN_DIR="$STAGE_DIR/usr/bin"
SYSTEMD_DIR="$STAGE_DIR/usr/lib/systemd/user"
DOC_DIR="$STAGE_DIR/usr/share/doc/lbright"
SYSTEMD_SYS_DIR="$STAGE_DIR/usr/lib/systemd/system"
LIBEXEC_DIR="$STAGE_DIR/usr/libexec/lbright"
POLKIT_DIR="$STAGE_DIR/usr/share/polkit-1/rules.d"

# Clean previous build artifacts
rm -rf dist/
mkdir -p "$DEBIAN_DIR"
mkdir -p "$BIN_DIR"
mkdir -p "$SYSTEMD_DIR"
mkdir -p "$DOC_DIR"
mkdir -p "$SYSTEMD_SYS_DIR"
mkdir -p "$LIBEXEC_DIR"
mkdir -p "$POLKIT_DIR"

echo "Staging files..."
# Install compiled binary
install -Dm755 target/release/lbright "$BIN_DIR/lbright"

# Systemd User Service unit (adjusted for global /usr/bin execution)
sed 's|%h/.local/bin/lbright|/usr/bin/lbright|g' lbright.service > "$SYSTEMD_DIR/lbright.service"
chmod 644 "$SYSTEMD_DIR/lbright.service"

# Ambient light sensor auto-heal: root oneshot reset unit + polkit rule letting the
# active local user start only that unit (used by the daemon and 'lbright als-reset').
install -Dm755 packaging/als-reset "$LIBEXEC_DIR/als-reset"
install -Dm644 packaging/lbright-als-reset.service "$SYSTEMD_SYS_DIR/lbright-als-reset.service"
install -Dm644 packaging/50-lbright.rules "$POLKIT_DIR/50-lbright.rules"

# Copyright / Documentation
if [ -f "LICENSE" ]; then
    cp LICENSE "$DOC_DIR/copyright"
fi
if [ -f "README.md" ]; then
    cp README.md "$DOC_DIR/"
fi

echo "Generating package metadata..."
# Create DEBIAN/control
cat << EOF > "$DEBIAN_DIR/control"
Package: lbright
Version: $VERSION
Section: utils
Priority: optional
Architecture: $ARCH
Maintainer: LeanBitLab <leanbitlab@users.noreply.github.com>
Depends: libc6
Recommends: ddcutil, i2c-tools, iio-sensor-proxy, polkitd | policykit-1
Provides: adaptive-brightness-linux, lbrightness
Replaces: adaptive-brightness-linux
Description: Lightweight adaptive screen brightness system for Linux
 High-performance Rust daemon for intelligent screen brightness
 control using direct sysfs kernel I/O, DDC/CI external monitor support,
 ALS ambient light smoothing, and an interactive ANSI terminal UI.
EOF

# Create DEBIAN/postinst
cat << 'EOF' > "$DEBIAN_DIR/postinst"
#!/bin/bash
set -e

if [ "$1" = "configure" ]; then
    echo "Reloading systemd..."
    systemctl daemon-reload 2>/dev/null || true

    # Enable the user service for every user (starts automatically at login)
    systemctl --global enable lbright.service 2>/dev/null || true

    # Start (or restart after an upgrade) in sessions that are already logged in
    for u in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user -M "$u@" daemon-reload 2>/dev/null || true
        home=$(getent passwd "$u" | cut -d: -f6)
        if [ -n "$home" ] && [ -e "$home/.config/systemd/user/lbright.service" ]; then
            echo "Note: $u also has a script-installed lbright (~/.local); run ./uninstall.sh as $u to avoid two installs."
        fi
        systemctl --user -M "$u@" restart lbright.service 2>/dev/null || true
    done

    echo ""
    echo "LBrightness (lbright) installed and started."
    echo "To open the interactive menu: lbright tui"
    echo "To check system status:       lbright status"
fi
EOF
chmod 755 "$DEBIAN_DIR/postinst"

# Create DEBIAN/prerm
cat << 'EOF' > "$DEBIAN_DIR/prerm"
#!/bin/bash
set -e

if [ "$1" = "remove" ] || [ "$1" = "deconfigure" ]; then
    # Stop running daemons in logged-in sessions (their monitor-sensor child exits with them)
    for u in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user -M "$u@" stop lbright.service 2>/dev/null || true
        systemctl --user -M "$u@" reset-failed lbright.service 2>/dev/null || true
    done
    systemctl --global disable lbright.service 2>/dev/null || true
    systemctl reset-failed lbright-als-reset.service 2>/dev/null || true
fi
EOF
chmod 755 "$DEBIAN_DIR/prerm"

# Create DEBIAN/postrm (reload systemd so the removed reset unit disappears)
cat << 'EOF' > "$DEBIAN_DIR/postrm"
#!/bin/bash
set -e

if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
    systemctl daemon-reload 2>/dev/null || true
    for u in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user -M "$u@" daemon-reload 2>/dev/null || true
    done
fi
if [ "$1" = "purge" ]; then
    # Debian policy forbids deleting files in users' home directories from a package.
    echo "Per-user configuration is kept in ~/.config/lbrightness and ~/.local/state/lbrightness."
    echo "Delete those directories manually for a complete cleanup."
fi
EOF
chmod 755 "$DEBIAN_DIR/postrm"

# Build the Debian package
PACKAGE_NAME="lbright_${VERSION}_${ARCH}.deb"
echo "Compiling Debian package: $PACKAGE_NAME..."
dpkg-deb --root-owner-group --build "$STAGE_DIR" "$PACKAGE_NAME"

echo ""
echo "=== Build Complete ==="
echo "Package generated: $PACKAGE_NAME"
echo "Install with:      sudo dpkg -i $PACKAGE_NAME"

