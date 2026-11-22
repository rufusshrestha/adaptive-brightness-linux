#!/bin/bash
# Uninstaller for LBrightness (lbright) - removes everything install.sh installed,
# including ~/.config/lbrightness (config.ini) and ~/.local/state/lbrightness.
# install.sh recreates a default config.ini.
#
# Options:
#   --keep-config   Keep ~/.config/lbrightness and ~/.local/state/lbrightness
set -e

PURGE=1
for arg in "$@"; do
    case "$arg" in
        --keep-config) PURGE=0 ;;
        -y|--purge) PURGE=1 ;;  # accepted for backward compatibility (now the default)
        -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "Unknown option: $arg"; echo "Usage: $0 [--keep-config]"; exit 1 ;;
    esac
done

USER_FILES=(
    "$HOME/.local/bin/lbright"
    "$HOME/.config/systemd/user/lbright.service"
    "$HOME/.config/systemd/user/default.target.wants/lbright.service"
)
ROOT_FILES=(
    /etc/systemd/system/lbright-als-reset.service
    /etc/polkit-1/rules.d/50-lbright.rules
    /usr/local/libexec/lbright/als-reset
)
DATA_DIRS=(
    "$HOME/.config/lbrightness"
    "$HOME/.local/state/lbrightness"
)

# Prints PIDs of this user's processes spawned by lbright (marked via LBRIGHT_CHILD=1).
lbright_children() {
    local env_file pid
    for env_file in /proc/[0-9]*/environ; do
        pid=${env_file#/proc/}; pid=${pid%/environ}
        [ -O "$env_file" ] || continue
        if tr '\0' '\n' 2>/dev/null < "$env_file" | grep -qx 'LBRIGHT_CHILD=1'; then
            echo "$pid"
        fi
    done
}

echo "=== Uninstalling LBrightness ==="

DEB_INSTALLED=0
if dpkg -s lbright >/dev/null 2>&1; then
    DEB_INSTALLED=1
    echo "Note: the lbright .deb package is installed. This removes only the script-installed"
    echo "(~/.local) copy and then starts the packaged lbright. To remove the package too:"
    echo "  sudo apt purge lbright"
fi

# 1. Stop the service and any other lbright processes (e.g. 'lbright daemon' run by hand)
systemctl --user stop lbright.service 2>/dev/null || true
systemctl --user disable lbright.service 2>/dev/null || true
systemctl --user reset-failed lbright.service 2>/dev/null || true
pkill -u "$USER" -x lbright 2>/dev/null || true

# 2. Stop leftover child processes (monitor-sensor normally exits with lbright already)
CHILDREN=$(lbright_children)
if [ -n "$CHILDREN" ]; then
    # shellcheck disable=SC2086
    kill $CHILDREN 2>/dev/null || true
fi
# Give processes up to 3s to exit before the final check
for _ in $(seq 30); do
    if ! pgrep -u "$USER" -x lbright >/dev/null && [ -z "$(lbright_children)" ]; then
        break
    fi
    sleep 0.1
done

# 3. Remove user files
rm -f "${USER_FILES[@]}"
systemctl --user daemon-reload 2>/dev/null || true

# 4. Remove the root sensor reset helper
# Returns 0 if the file exists, 1 if absent, 2 if unknown. Some dirs (e.g.
# /etc/polkit-1/rules.d) aren't readable without root, so a plain [ -e ] would
# wrongly report "absent". Fall back to non-interactive sudo there.
root_file_state() {
    local dir
    dir=$(dirname "$1")
    if [ ! -e "$dir" ]; then
        return 1
    elif [ -r "$dir" ] && [ -x "$dir" ]; then
        [ -e "$1" ] && return 0 || return 1
    elif sudo -n true 2>/dev/null; then
        sudo -n test -e "$1" && return 0 || return 1
    fi
    return 2
}

# install.sh installs the polkit rule LAST and we remove it FIRST, so if the
# (readable) unit and script are both absent, the rule is absent too. That avoids
# a sudo prompt on --user-only installs just to look inside /etc/polkit-1/rules.d.
NEED_ROOT=0
for f in "${ROOT_FILES[@]}"; do
    rc=0; root_file_state "$f" || rc=$?
    [ "$rc" -eq 0 ] && NEED_ROOT=1
done
if [ "$NEED_ROOT" -eq 1 ]; then
    echo "Removing ambient light sensor reset helper (requires sudo)..."
    sudo rm -f /etc/polkit-1/rules.d/50-lbright.rules
    sudo systemctl reset-failed lbright-als-reset.service 2>/dev/null || true
    sudo rm -f "${ROOT_FILES[@]}"
    sudo rmdir /usr/local/libexec/lbright 2>/dev/null || true
    sudo systemctl daemon-reload
fi

# 5. Configuration and state
if [ "$PURGE" -eq 1 ]; then
    rm -rf "${DATA_DIRS[@]}"
    echo "Removed configuration (~/.config/lbrightness/config.ini) and state files."
else
    echo "Kept configuration at ~/.config/lbrightness (state at ~/.local/state/lbrightness)."
fi

# 6. Verify nothing is left behind
LEFT=()
for f in "${USER_FILES[@]}"; do
    [ -e "$f" ] && LEFT+=("file: $f")
done
for f in "${ROOT_FILES[@]}" /usr/local/libexec/lbright; do
    rc=0; root_file_state "$f" || rc=$?
    [ "$rc" -eq 0 ] && LEFT+=("file: $f")
    # Unknown (unreadable dir, no cached sudo) only matters if we removed root files
    [ "$rc" -eq 2 ] && [ "$NEED_ROOT" -eq 1 ] && LEFT+=("unverified (needs sudo to check): $f")
done
if [ "$PURGE" -eq 1 ]; then
    for d in "${DATA_DIRS[@]}"; do
        [ -e "$d" ] && LEFT+=("dir: $d")
    done
fi
for pid in $(pgrep -u "$USER" -x lbright) $(lbright_children); do
    LEFT+=("process: $pid ($(ps -o args= -p "$pid" 2>/dev/null))")
done

if [ ${#LEFT[@]} -eq 0 ]; then
    echo "Uninstallation complete! Nothing left behind."
else
    echo "Uninstallation finished, but these remain:"
    printf '  %s\n' "${LEFT[@]}"
    exit 1
fi

# 7. Hand over to the packaged lbright, which the script install was overriding
if [ "$DEB_INSTALLED" -eq 1 ]; then
    systemctl --user daemon-reload 2>/dev/null || true
    if systemctl --user restart lbright.service 2>/dev/null; then
        echo "Started the packaged lbright (/usr/bin/lbright)."
    else
        echo "Warning: could not start the packaged lbright. Try: systemctl --user restart lbright.service"
    fi
fi
