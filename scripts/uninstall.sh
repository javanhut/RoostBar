#!/bin/sh
# Remove roostbar: the running bar, the binary, and the autostart line.
# Config and the audio/Bluetooth packages are left alone unless asked:
#
#   ./uninstall.sh              remove bar + binary + autostart line
#   ./uninstall.sh --purge      also delete ~/.config/roostbar
#   ./uninstall.sh --packages   also `rvn uninstall` the packages install.sh
#                               added and the bluetoothd init drop-in (Raven only)
set -eu

# Run as the user, not root: cargo, ~/.local/bin and ~/.config are all per-user,
# and the few root steps below call sudo themselves. `sudo ./install.sh` (or
# `sudo imlazy install`) is re-executed as the invoking user.
if [ "$(id -u)" = 0 ]; then
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        exec sudo -u "$SUDO_USER" -H -- "$0" "$@"
    fi
    echo "run this as your normal user, not root" >&2
    exit 1
fi

PURGE=0; PACKAGES=0
for a in "$@"; do
    case "$a" in
        --purge) PURGE=1 ;;
        --packages) PACKAGES=1 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

OS_ID=""
[ -r /etc/os-release ] && OS_ID="$(. /etc/os-release; printf '%s' "${ID:-}")"

# 1. Stop the running bar.
if pkill -x roostbar 2>/dev/null; then
    echo "stopped roostbar"
fi

# 2. Binary.
if [ -e "$HOME/.local/bin/roostbar" ]; then
    rm -f "$HOME/.local/bin/roostbar"
    echo "removed ~/.local/bin/roostbar"
fi

# 3. Autostart line in the session script (the one install.sh told you to add).
SESSION=/usr/bin/raven-wayland-session
if [ -r "$SESSION" ] && grep -q 'local/bin/roostbar' "$SESSION"; then
    echo "removing autostart line from $SESSION"
    sudo sed -i '/local\/bin\/roostbar/d' "$SESSION"
fi

# 4. Config, on request.
if [ "$PURGE" = 1 ] && [ -d "$HOME/.config/roostbar" ]; then
    rm -rf "$HOME/.config/roostbar"
    echo "removed ~/.config/roostbar"
fi

# 5. Packages and the bluetoothd service, on request, Raven only.
if [ "$PACKAGES" = 1 ]; then
    if [ "$OS_ID" = "raven" ]; then
        if [ -e /etc/raven/init.d/bluetoothd.toml ]; then
            sudo raven-rc stop bluetoothd 2>/dev/null || true
            sudo rm -f /etc/raven/init.d/bluetoothd.toml
            sudo raven-rc reload 2>/dev/null || true
            echo "removed bluetoothd from raven-init"
        fi
        pkill -x pipewire-pulse 2>/dev/null || true
        pkill -x wireplumber 2>/dev/null || true
        pkill -x pipewire 2>/dev/null || true
        installed=""
        for p in pipewire-audio wireplumber pipewire-pulse bluez; do
            pacman -Q "$p" >/dev/null 2>&1 && installed="$installed $p"
        done
        if [ -n "$installed" ]; then
            echo "Raven Linux: uninstalling$installed"
            # shellcheck disable=SC2086
            sudo rvn uninstall -y $installed
        fi
    else
        echo "--packages is only automated on Raven Linux (ID=${OS_ID:-unknown}); remove pipewire-audio wireplumber pipewire-pulse bluez yourself"
    fi
fi

echo "roostbar uninstalled."
