#!/bin/sh
# Build and install roostbar for the current user, install the audio and
# Bluetooth stacks it drives when the OS is Raven Linux, and print the
# autostart line.
set -eu
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

cat <<MSG

Installed ~/.local/bin/roostbar and ~/.config/roostbar/config.toml.

Run it now:        roostbar &

Autostart: Huginn has no autostart hook, and the session script is system
owned. Add one line before the final \`exec "\${COMPOSITOR}"\` in
/usr/bin/raven-wayland-session:

    sudo sed -i 's|^exec "\${COMPOSITOR}" --backend udev|[ -x "\$HOME/.local/bin/roostbar" ] \&\& (sleep 1; "\$HOME/.local/bin/roostbar") \&\n&|' /usr/bin/raven-wayland-session
MSG
