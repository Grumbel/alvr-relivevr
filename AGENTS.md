# AGENTS.md: ALVR ReliveVR

## What this is

A permanent fork of [ALVR](https://github.com/alvr-org/ALVR). Its SteamVR
driver streams to AMD's **ReliveVR** Android client (`com.amd.wirelessgvr`,
Lenovo Mirage Solo / Daydream) instead of ALVR's own headset app. Linux only,
built with Nix. User-facing overview: [README.md](README.md). Open items:
[TODO.md](TODO.md).

The protocol was reverse-engineered in the sibling project
[relivevr-linux](https://github.com/Grumbel/relivevr-linux) (standalone test
server, `docs/protocol.md`, `docs/re-notes.md`, `docs/client-apk.md`).
Protocol questions are answered there, and new protocol findings belong
there too.

## Repository layout

- Published at `https://github.com/Grumbel/alvr-relivevr` (remote `origin`).
  ALVR itself is remote `upstream` (only `master` is fetched).
- `master`: the fork. ALVR master plus the ReliveVR commits on top. Commit
  here.
- `relivevr-20.14.1`: the first attempt on ALVR v20.14.1. It's kept for
  reference and isn't maintained. It was dropped because 20.14.1 captures
  frames with a Vulkan layer that loses the render pose, which caused head
  tracking jitter.
- To follow upstream, rebase `master` onto `upstream/master`.
  `git log upstream/master..master` lists the fork's own commits.
- `openvr` is a git submodule. The flake needs it
  (`inputs.self.submodules = true`).

## Where the ReliveVR code lives

- `alvr/server_core/src/relivevr.rs`: the whole client protocol (UDP 1235,
  FlowCtrl framing, Hello / StartRequest / StartSensor / VideoInit /
  VideoData, DeviceEvent poses and buttons). `lib.rs` calls
  `relivevr::handshake_loop` in place of ALVR's
  `connection::handshake_loop`, which is left in place as dead code to keep
  rebases small.
- `alvr/server_openvr/cpp/platform/linux/`: per-eye VAAPI encoding (two
  pipelines cropping the side-by-side frame, shared IDR decision), sRGB
  encode in `quad.comp`, per-eye texture bounds.
- `alvr/server_openvr/src/props.rs`: Vive wand emulation and its input
  profile.
- `alvr/server_openvr/build.rs`: `ALVR_FFMPEG_DIR`, `ALVR_X264_DIR`,
  `ALVR_VULKAN_HEADERS_DIR` (Nix instead of `cargo xtask prepare-deps`).
- `flake.nix`: package, apps, dev shell. Built with crane: the
  dependencies are a separate derivation (`.#alvr-deps`) that only rebuilds
  when `Cargo.toml`/`Cargo.lock` change. If `nix build` starts recompiling
  third-party crates again, the two derivations' environments differ (see
  the comments in `commonArgs`). nixpkgs is pinned to the
  developer's NixOS revision **on purpose**: SteamVR loads the driver with
  the host glibc.

Keep changes to upstream files small and local. Rebases onto ALVR master
should stay cheap.

## Hard-won rules (don't regress these)

- **StartSensor is sent exactly once.** A second one restarts the client
  pipeline and leaves Present stuck on frame 0.
- **`ptsSensor` never goes backwards.** A successful pose lookup on the client
  erases every older pose. Frames carry the timestamp of the pose they were
  warped to.
- **H.264 only.** The client always decodes avc, whatever VideoInit says.
- The Mirage Solo config (1440² per eye, 75 Hz, symmetric 100° FOV, no
  distortion on the PC) goes out as a fixed `SteamvrHmdInitConfig` with a
  matching restart-settings hash. If it differs from what SteamVR already
  runs with, every connect restarts SteamVR.
- On NixOS, vrserver runs in Steam's bubblewrap sandbox (private `/run/user`,
  separate mount namespace from the compositor). Don't add IPC that relies
  on paths shared between the two.
- Build against the system's nixpkgs. A newer glibc than the host's makes
  SteamVR refuse the driver.

## Build and test

```bash
nix develop -c cargo xtask build-streamer --release   # → build/alvr_streamer_linux
nix run .#register-dev-driver                         # SteamVR uses that build
nix run .#steamvr                                     # or: nix run . → "Launch SteamVR"
nix run .#restart-client                              # headset app reconnects
nix run .#logcat                                      # client log (AMF_TRACE)
```

- The headset is reachable via adb (Mirage Solo, serial `HAD1E8GL`).
- Server log: `~/alvr_session_log.txt` (enable file logging in the
  dashboard). SteamVR logs: `~/.steam/root/logs/vrserver.txt`,
  `vrcompositor.txt`.
- Headset screenshots land in `/sdcard/Pictures/Screenshot_*.jpg`
  (`adb pull`).
- Health check in logcat: `[DisplayPipeline] Average latency: …`.
  `dec_subm_interval` should be ≈13.3 ms (75 fps). `Pose for Present … not
  found` means pts or ptsSensor are wrong.
- Only the user can judge jitter, brightness and controller feel on the
  headset. Ask them, and don't declare such fixes done before they confirm.

## Conventions

- Author: Ingo Ruhnke <grumbel@gmail.com>. Commit directly to `master` with a
  `Co-Authored-By:` trailer naming the model that did the work.
- Fork code is MIT, like ALVR.
- Update TODO.md when something significant changes.
