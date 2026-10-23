# ALVR ReliveVR

Play SteamVR games on Linux with a **Lenovo Mirage Solo** (Daydream), using
AMD's official **ReliveVR** headset app (Radeon ReLive for VR / Wireless GVR,
`com.amd.wirelessgvr`) as the client.

This is a fork of [ALVR](https://github.com/alvr-org/ALVR). It keeps ALVR's
SteamVR driver, rendering and encoding, and swaps ALVR's own client protocol
for the ReliveVR one. AMD only ever shipped a Windows server for ReliveVR;
this makes Linux the server side instead. The protocol was
reverse-engineered in
[relivevr-linux](https://github.com/Grumbel/relivevr-linux), which also has a
standalone test server and the protocol documentation.

ALVR's own headset apps can't connect to this build: the driver only speaks
ReliveVR.

## Status

Working on the test setup (NixOS, AMD RX 6700, SteamVR 2.16.7):

- SteamVR environment and dashboard on the headset at 1440×1440 per eye,
  75 fps, about 26 ms end-to-end (client statistics).
- Head tracking without jitter: the server warps each frame to the newest
  headset pose and tags it with that pose's timestamp.
- The Daydream controller acts as a right-hand Vive wand:

  | Daydream | Vive wand |
  |----------|-----------|
  | Touchpad (touch, click, position) | Trackpad |
  | Client-reported trigger | Trigger |
  | App (short or long press) | System (SteamVR dashboard) |
  | Volume + | Menu |
  | Volume − | Grip |
  | Home | Handled by Daydream itself, never reaches SteamVR |

Open issues are in [TODO.md](TODO.md).

## Requirements

- **Headset:** Lenovo Mirage Solo with the ReliveVR app
  (`com.amd.wirelessgvr` 1.0.13). See
  [Getting the ReliveVR app](#getting-the-relivevr-app).
- **PC:** x86_64 Linux with Nix. Tested only on NixOS.
- **GPU:** VAAPI H.264 encoding (AMD; Intel should work but is untested).
  Each eye gets its own VAAPI encoder.
- **Network:** PC and headset on the same LAN. The client finds the server by
  UDP broadcast on port **1235**, so that port must be open.
- **Steam with SteamVR.**

The ReliveVR client always decodes H.264. HEVC gives a green screen, so the
codec is fixed to H.264.

## Getting the ReliveVR app

AMD no longer distributes the headset app. It's archived on APKMirror:
<https://www.apkmirror.com/apk/amd-advanced-micro-devices-inc/radeon-relive-for-vr/>. Pick **`com.amd.wirelessgvr` 1.0.13** (versionCode 13, arm64-v8a +
armeabi-v7a), the Daydream build tested here. AMD's other ReliveVR apps
(`com.amd.wirelesshmd` for Oculus, `com.amd.wirelessvive` for Vive Focus)
are untested. The APK is AMD's proprietary software and isn't redistributed
in this repository.

Check the download and install it over adb (developer mode on the headset):

```bash
sha256sum com.amd.wirelessgvr_1.0.13*.apk
# 992b0ccd9c02e5efd876e6ff0b4fe477d3d37761429069cfaf8976ad4ff62496
adb install com.amd.wirelessgvr_1.0.13*.apk
```

## Quick start

```bash
# Build the driver, then start ALVR's dashboard
nix run github:Grumbel/alvr-relivevr

# In the dashboard: "Launch SteamVR". This registers the driver with SteamVR.
# Then start the ReliveVR app on the headset; it connects on its own.
```

The first connect restarts SteamVR once while the driver applies the fixed
Mirage Solo configuration (resolution, refresh rate, FOV).

### Flake apps

| App | What it does |
|-----|--------------|
| `nix run .` / `.#dashboard` | ALVR dashboard: settings, logs, "Launch SteamVR" |
| `nix run .#steamvr` | Register the driver and start SteamVR through Steam, no dashboard needed |
| `nix run .#register-driver` | Register this flake's driver in `~/.config/openvr/openvrpaths.vrpath` and unregister every other ALVR driver |
| `nix run .#register-dev-driver` | Same for a development build in `./build/alvr_streamer_linux` (see below) |
| `nix run .#restart-client` | Restart the ReliveVR app on the headset over adb |
| `nix run .#logcat` | Follow the ReliveVR client's log over adb |

The registered driver path is a Nix store path. After `nix store gc` it can
disappear, so register it again (any of the first four apps does that).

## Important: nixpkgs must match your system

SteamVR loads the driver with the host's glibc. A driver built against a
newer glibc fails to load (`GLIBC_2.43 not found`). For that reason
`flake.nix` pins nixpkgs to the author's NixOS revision. If your system is
older than that revision, point the input at your own system's nixpkgs:

```bash
nix run . --override-input nixpkgs github:NixOS/nixpkgs/$(nixos-version --revision)
```

## SteamVR setup (NixOS)

This is the configuration the fork was tested with:

- Steam launch option for SteamVR:
  `~/.steam/root/steamapps/common/SteamVR/bin/vrmonitor.sh %command%`
- In `~/.steam/root/config/steamvr.vrsettings`, section `steamvr`:
  - `"supersampleManualOverride": true, "supersampleScale": 1.0`: render at
    the stream resolution. The default 150% pushes render plus encode past
    the 13.3 ms frame budget.
  - `"enableHomeApp": false`: SteamVR Home still stutters (see TODO.md).
  - `"disableLinuxWaitForPresent": true`: was needed with the earlier
    v20.14.1-based version. It hasn't been re-checked since the port to
    ALVR master.
- SteamVR asking for superuser access during setup (`setcap` on
  `vrcompositor-launcher`) fails inside Steam's sandbox on NixOS. It's
  harmless. To silence it, run this once outside Steam:
  `sudo setcap CAP_SYS_NICE+ep ~/.steam/root/steamapps/common/SteamVR/bin/linux64/vrcompositor-launcher`

Logs: ALVR writes `~/alvr_session_log.txt` when file logging is enabled in
the dashboard. SteamVR's logs are in `~/.steam/root/logs/`.

## Development

```bash
nix develop
cargo xtask build-streamer --release   # → build/alvr_streamer_linux
nix run .#register-dev-driver          # point SteamVR at that build
```

The ReliveVR code:

| Path | Contents |
|------|----------|
| `alvr/server_core/src/relivevr.rs` | ReliveVR protocol: discovery, handshake, StartSensor, VideoInit, per-eye VideoData, poses, controller input |
| `alvr/server_core/src/lib.rs` | Starts `relivevr::handshake_loop` instead of ALVR's `connection::handshake_loop` |
| `alvr/server_openvr/cpp/platform/linux/` | Two VAAPI encoders, each cropping one half of the side-by-side frame; sRGB encode of the compositor's linear frames |
| `alvr/server_openvr/src/props.rs` | Mirage Solo HMD and Vive wand properties |
| `flake.nix` | FFmpeg 8.1 with ALVR's VAAPI patches, package, apps, dev shell |

See [AGENTS.md](AGENTS.md) for the fork's branch layout and how to work on it.

## Branches

- `master`: the fork, rebased onto ALVR master.
- `relivevr-20.14.1`: the first version, on ALVR v20.14.1 (the version
  nixpkgs packages). Abandoned because of head-tracking jitter. 20.14.1's
  Vulkan-layer capture doesn't deliver the pose each frame was rendered
  with, and ALVR master's direct-mode API does.

## License

MIT, like ALVR ([LICENSE](LICENSE)). ALVR's original README covers the
upstream project: <https://github.com/alvr-org/ALVR>.
