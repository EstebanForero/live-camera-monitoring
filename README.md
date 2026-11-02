# Live camera monitoring using Rust

A Linux desktop app for a TP-Link Tapo C200. The picture is pulled straight from the camera on your network. The app does not sign in to TP-Link cloud, and it does not phone home.

The window is a [Tauri](https://tauri.app/) shell. Rust opens the RTSP stream, speaks ONVIF for pan and tilt, and watches the picture for movement.

## What you can do

- Watch the live picture. **Smooth** is the smaller stream. **HD** is 1080p.
- Pan and tilt. Hold a direction to keep turning, or tap once to nudge. The C200 has no zoom.
- Save named views and jump back to them. **Home** returns to the camera's home position, when one is set.
- Pop the picture out of the controls. That window can stay on top, and double-click makes it fullscreen.
- Get a desktop notification when the picture changes. Alerts are at least 20 seconds apart. Turning the camera does not count.
- Draw red outlines around what moved, in the popped-out view. Outlines and notifications are separate switches, so you can use either one alone.
- Save a snapshot under `~/Pictures/Camera Live/`.

Keyboard, when the main window is focused: arrow keys move, `H` goes home, `S` saves a snapshot, `F` toggles fullscreen.

## How it is laid out

`ui/` is the interface: the HTML, CSS, and JavaScript for the two windows. There is no Node app and no bundler. Tauri embeds those files in the binary, so the folder has to stay.

`src/` is the Rust side.

| File | Role |
| --- | --- |
| `stream.rs` | RTSP over TCP, decode, JPEG frames into the window |
| `onvif.rs` | Local ONVIF on port 2020: profiles, pan, tilt, presets |
| `motion.rs` | Picture diff for alerts and outlines |
| `config.rs` | Settings file, mode `0600` |

Icons live in `icons/`. Window permissions live in `capabilities/`.

## Requirements

- Linux with a desktop session that can show notifications
- Rust 1.77 or newer
- GStreamer 1.x, including the base, good, bad, and libav plugin packages

On Arch those GStreamer packages are `gstreamer`, `gst-plugins-base`, `gst-plugins-good`, `gst-plugins-bad`, and `gst-libav`. On Debian and Ubuntu:

```sh
sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav
```

The camera needs its own local account. In the Tapo app: camera, Settings, Advanced Settings, Camera Account. That username and password are not your TP-Link login. The computer and the camera have to be on the same network. RTSP is port 554. ONVIF on a C200 is port 2020.

## Run

```sh
cargo run --release
```

The first screen asks for the camera address and the camera account. Settings are stored in `~/.config/camera-live/config.json`. That file holds the password, so it is not part of this repository.

`cargo test` covers the URL parser, the ONVIF reads, JPEG sizing, and the movement boxes.

## Install the binary

```sh
cargo build --release
install -m 755 target/release/camera-live ~/.local/bin/camera-live
```

A desktop entry in `~/.local/share/applications/` makes it show up in an app searcher. Point `Exec` at that binary and `Icon` at `icons/icon.png`.
