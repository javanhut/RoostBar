#!/bin/sh
# Build and install roostbar for the current user, install the audio and
# Bluetooth stacks it drives when the OS is Raven Linux, and print the
# autostart line.
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
cd "$(dirname "$0")/.."   # repo root, whether the script lives in ./ or ./scripts/

# --- which OS is this? ---------------------------------------------------
OS_ID=""
[ -r /etc/os-release ] && OS_ID="$(. /etc/os-release; printf '%s' "${ID:-}")"

# Packages roostbar needs at runtime, beyond what the base image ships.
#   pipewire-audio  SPA ALSA/audioconvert plugins -- without them PipeWire has no devices
#   wireplumber     session manager
#   pipewire-pulse  PulseAudio compatibility for browsers etc.
#   bluez           bluetoothd, driven over D-Bus
PKGS="pipewire-audio wireplumber pipewire-pulse bluez"

if [ "$OS_ID" = "raven" ]; then
    missing=""
    for p in $PKGS; do
        pacman -Q "$p" >/dev/null 2>&1 || missing="$missing $p"
    done
    if [ -n "$missing" ]; then
        echo "Raven Linux: installing$missing"
        # shellcheck disable=SC2086
        sudo rvn install -y $missing
    else
        echo "Raven Linux: audio and Bluetooth packages already installed"
    fi
    if [ ! -e /etc/raven/init.d/bluetoothd.toml ]; then
        echo "Raven Linux: registering bluetoothd with raven-init"
        sudo cp contrib/bluetoothd.toml /etc/raven/init.d/
        sudo raven-rc reload
        sudo raven-rc start bluetoothd || true
    fi
else
    echo "Not Raven Linux (ID=${OS_ID:-unknown}); install these yourself: $PKGS"
fi

# --- build & install -----------------------------------------------------
cargo build --release
mkdir -p "$HOME/.local/bin" "$HOME/.config/roostbar"
install -m755 target/release/roostbar "$HOME/.local/bin/roostbar"
[ -e "$HOME/.config/roostbar/config.toml" ] || cp config.example.toml "$HOME/.config/roostbar/config.toml"

# --- per-session autostart ---------------------------------------------
# The bar is a per-user Wayland client, so it belongs in the session, not in
# raven-init. raven-wayland-session gets a session.d mechanism (see
# contrib/raven-wayland-session.patch) and the bar a drop-in in the user's
# session.d. Applying the patch is the one root step; it is skipped once the
# marker is present.
SESSION=/usr/bin/raven-wayland-session
if [ -r "$SESSION" ] && ! grep -q '>>> session.d >>>' "$SESSION"; then
    echo "adding session.d support to $SESSION (needs sudo)"
    sudo cp "$SESSION" "$SESSION.orig"
    sudo patch -s "$SESSION" contrib/raven-wayland-session.patch
    sudo sh -n "$SESSION"
fi
# Undo the older single-line autostart if it was ever added.
if [ -r "$SESSION" ] && grep -q 'local/bin/roostbar' "$SESSION"; then
    sudo sed -i '/local\/bin\/roostbar/d' "$SESSION"
fi
SESSION_D="${XDG_CONFIG_HOME:-$HOME/.config}/raven/session.d"
mkdir -p "$SESSION_D"
install -m755 contrib/session.d/50-roostbar "$SESSION_D/50-roostbar"

if ! pgrep -x roostbar >/dev/null 2>&1; then
    setsid "$HOME/.local/bin/roostbar" </dev/null >/dev/null 2>&1 &
    echo "started roostbar"
fi

cat <<MSG

Installed:
  ~/.local/bin/roostbar
  ~/.config/roostbar/config.toml
  $SESSION_D/50-roostbar        (starts the bar at every login)

Anything executable you drop into $SESSION_D
starts with your session the same way.
MSG
