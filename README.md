# Whisp

Whisp is a small Wayland-native on-screen display for volume and brightness changes, written in Rust.
It uses `gtk4-layer-shell`, so it talks to Wayland layer-shell directly and does not provide an X11 backend.

## Features

- Wayland-only overlay surface
- Lightweight daemon plus client command for efficient repeated updates
- Volume and brightness event types with percentage bars
- Per-event mute state and timeout overrides
- Nix flake for reproducible build and development

## Build

```bash
nix build
```

Or:

```bash
nix develop
cargo build
```

## Usage

Start the daemon once from your compositor config:

```bash
whisp daemon
```

Send OSD events from keybindings or scripts:

```bash
whisp show volume 42
whisp show volume 0 --muted
whisp show brightness 65
```

Default socket path:

```text
$XDG_RUNTIME_DIR/whisp.sock
```

You can override it with `--socket /path/to/socket` on both `daemon` and `show`.

## Example Keybinding Hooks

```bash
wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+
whisp show volume "$(wpctl get-volume @DEFAULT_AUDIO_SINK@ | awk '{ print int($2 * 100) }')"
```

```bash
brightnessctl set 5%+
whisp show brightness "$(brightnessctl -m | awk -F, '{ print int($4) }' | tr -d '%')"
```
