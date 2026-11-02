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

- A Linux desktop session. Notifications use the session bus.
- Rust 1.77 or newer. If `cargo` is missing: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`, then open a new terminal.
- WebKitGTK 4.1, GTK 3, and GStreamer with the base, good, bad, and libav plugins.

Arch:

```sh
sudo pacman -S --needed \
  webkit2gtk-4.1 gtk3 base-devel openssl \
  gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav
```

Debian or Ubuntu:

```sh
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libssl-dev \
  build-essential curl wget file librsvg2-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav
```

The camera needs its own local account. In the Tapo app: camera, Settings, Advanced Settings, Camera Account. That username and password are not your TP-Link login. This computer and the camera have to be on the same network. RTSP is port 554. ONVIF on a C200 is port 2020.

## Run it

Clone the repo and start the app from that folder:

```sh
git clone https://github.com/EstebanForero/live-camera-monitoring.git
cd live-camera-monitoring
cargo run --release
```

The first build takes a few minutes. When the window opens, choose **Camera** and enter the camera's IP address plus the camera account. **Smooth** is the usual stream. **HD** is 1080p and uses more bandwidth.

Settings are saved to `~/.config/camera-live/config.json` (mode `0600`). That file holds the password, so the next launch connects on its own. The file is not part of this repository.

`cargo run` without `--release` also works. It compiles faster and the window is a bit heavier.

`cargo test` checks the URL parser, the ONVIF reads, JPEG sizing, and the movement boxes.

## Open it with a command

Build once, then copy the binary onto your `PATH`:

```sh
cargo build --release
mkdir -p ~/.local/bin
install -m 755 target/release/camera-live ~/.local/bin/camera-live
```

`~/.local/bin` is already on the `PATH` for most desktops. Open a new terminal and run:

```sh
camera-live
```

To show it in an app searcher, save this as `~/.local/share/applications/camera-live.desktop`. Change the home path if your username is not the one in `Exec` and `Icon`.

```ini
[Desktop Entry]
Type=Application
Name=Camera Live
Comment=Live view of a Tapo C200
Exec=/home/YOU/.local/bin/camera-live
Icon=/home/YOU/live-camera-monitoring/icons/icon.png
Terminal=false
Categories=AudioVideo;
StartupWMClass=camera-live
```

Then refresh the launcher index:

```sh
update-desktop-database ~/.local/share/applications
```

Search for **Camera Live**.
