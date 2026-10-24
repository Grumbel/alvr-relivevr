# TODO

## Verify on the headset

- Pose match in real games, at low fps and after SteamVR recenters (fixed
  2026-10-24, checked only in the SteamVR environment so far). The log should
  show `PoseHistory: match … 0 inexact`. Any `no exact pose match` warning
  means frames carry the wrong pose (jitter).
- Laser pointer and controller highlight in the SteamVR status window after
  switching the Vive wand to its own input profile (`a88661a`). Before that,
  SteamVR dropped the dashboard bindings
  (`Invalid input type position … /input/joystick`). If the pointer is still
  missing, an active gamepad (SteamVR `gamepad` driver) can make the
  dashboard switch to gamepad navigation.
- Is `disableLinuxWaitForPresent` still needed on ALVR master?

## Open

- SteamVR Home stutters (looks like missed frames); the plain SteamVR
  environment is smooth. Home is disabled for now (`enableHomeApp: false`).
- SteamVR shows "SteamVR Fail (-203)" at startup and the desktop VR View
  stays black, although rendering and streaming work.
- Driver unload trips SteamVR's watchdog on shutdown.
- Cleanup: debug dump of the first left-eye frames to `~/relivevr_left.h264`
  (`relivevr.rs`), timing and pose-match diagnostics at Info level.
- Rebase onto current `upstream/master`. Upstream `adaa16b` ("colours too dark
  on Linux") overlaps with our sRGB encode in `quad.comp` (`b24b96d`), so
  keep only one of them.
- Follow the client's UpdateRequest `FrameRate` instead of the fixed 75 Hz.
- HEVC: the client ignores VideoInit `CodecID`. Find what makes it request
  hevc in StartRequest (see relivevr-linux `docs/re-notes.md`).
- Only VAAPI has the per-eye encode path; NVENC and software are untested.
