# RoostBar

A thin, glanceable status bar for **Huginn** on **Raven Linux**. One strip,
26 px, semi-transparent, sitting in a `zwlr_layer_shell` exclusive zone so no
window ever hides it:

```
 Thu 28 Aug                     󰤨 Hutchinson6   󰂱 WH-1000XM4   󰕾 45%   󰁹 99%   14:32
```

Left: date. Right: Wi‑Fi (SSID via `caw`), Bluetooth, volume, battery, clock.
Software-rendered with the JetBrains Mono Nerd Font already on Raven; no GTK,
no Qt, ~4 MB binary, negligible CPU (redraws only when something changes).

## Interaction

| Where      | Action        | Effect                                              |
|------------|---------------|-----------------------------------------------------|
| Volume     | scroll        | ±5 % (`volume_step`)                                |
| Volume     | click         | toggle mute                                         |
| Bluetooth  | left click    | connected → disconnect; off → power on; idle → connect to `bluetooth_device`, else a trusted paired device |
| Bluetooth  | middle/right  | toggle adapter power                                |

Huginn's keybindings are hardcoded and don't include volume keys, so there is
a CLI that uses the same backend as the bar:

```
roostbar vol up | down | mute | get
```

## Audio: PipeWire with an ALSA fallback

The bar prefers **PipeWire** (default sink's `Props`, cubic volume mapping
like `wpctl`) and falls back to plain **ALSA** (`Master` on the first card
with a Speaker/Headphone element — the laptop codec, not HDMI). It re-checks
on every slow poll, so it switches to PipeWire the moment it appears.

Raven has no systemd user session, so the bar starts PipeWire itself at
launch (`start_pipewire = true`) — the standard `pipewire` + `wireplumber` +
`pipewire-pulse` trio if wireplumber is installed, otherwise PipeWire's own
session-manager-free `minimal.conf` (patched so it doesn't need
`pipewire-pulse`). Either way it needs the SPA audio plugins:

```
sudo rvn install -y pipewire-audio wireplumber pipewire-pulse
```

The PipeWire backend drives PipeWire's own `pw-dump -m` (a live JSON stream
of every object and every change) and `pw-cli set-param`, so it builds with
no libpipewire bindings. A native libpipewire backend is in
`src/pipewire_audio.rs` behind `--features pipewire-native`; it needs
`libclang` at build time (`sudo rvn install -y clang`).

## Bluetooth

Raven's base image has no Bluetooth stack. The bar shows `󰂲 —` until
`org.bluez` is on the system bus. To get it:

```
sudo rvn install -y bluez
sudo cp contrib/bluetoothd.toml /etc/raven/init.d/
sudo raven-rc reload && sudo raven-rc start bluetoothd
```

Pair once with `bluetoothctl` (`scan on`, `pair`, `trust`); after that the
bar's icon connects/disconnects with a click.

## Build & install

```
./scripts/install.sh            # on Raven Linux: `sudo rvn install -y` the audio/BT packages,
                        # register bluetoothd; then build, copy to ~/.local/bin, seed config
roostbar &              # run now
```

`scripts/install.sh` reads `/etc/os-release`; when `ID=raven` it installs
`pipewire-audio wireplumber pipewire-pulse bluez` with `sudo rvn install -y`
and drops `contrib/bluetoothd.toml` into `/etc/raven/init.d`. On anything
else it just tells you what to install.

`scripts/install.sh` prints the one `sudo sed` line that adds the bar to
`/usr/bin/raven-wayland-session`, the only place a Huginn session starts
programs from.

Uninstall:

```
./scripts/uninstall.sh              # stop the bar, remove the binary and the autostart line
./scripts/uninstall.sh --purge      # also delete ~/.config/roostbar
./scripts/uninstall.sh --packages   # also `sudo rvn uninstall -y` the packages install.sh added
                            # and unregister bluetoothd (Raven Linux only)
```

Config: `~/.config/roostbar/config.toml` — see `config.example.toml`.
Debug: `ROOSTBAR_DEBUG=1 roostbar` logs backend choice, size, volume changes.

## Layout of the code

- `src/main.rs` — layer-shell surface, calloop event loop, pointer input, modules & layout
- `src/render.rs` — ARGB canvas + `ab_glyph` text
- `src/pipewire_cli.rs` — PipeWire backend via `pw-dump -m` / `pw-cli`
- `src/pipewire_audio.rs` — native libpipewire backend (optional feature)
- `src/audio.rs` — ALSA fallback
- `src/bluetooth.rs` — BlueZ over D-Bus (`zbus`), actions on a worker thread
- `src/system.rs` — battery (sysfs), Wi‑Fi (`caw status`)
