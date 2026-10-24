# TODO

## Verify on the headset

- Laser pointer and controller highlight in the SteamVR status window after
  switching the Vive wand to its own input profile (`a88661a`). Before that,
  SteamVR dropped the dashboard bindings
  (`Invalid input type position … /input/joystick`). If the pointer is still
  missing, an active gamepad (SteamVR `gamepad` driver) can make the
  dashboard switch to gamepad navigation.
- Is `disableLinuxWaitForPresent` still needed on ALVR master?

## Open

- Jitter after a SteamVR recenter. Each `Update ZeroPose for the chaperone
  universe` in vrserver.txt changes the transform on the app poses, and the
  PoseHistory match goes from exact (diff ~1e-10) to 6°–50° off, picking
  samples up to 250 ms old. The warp then starts from the wrong pose, and the
  frame is stamped with it. `SetTransform` (chaperone working copy, on events)
  doesn't catch it, and `m_correction` converges slowly to a wrong value (8.55°
  for a ~6° offset). relivevr-linux showed that a stamp/image mismatch is
  exactly what makes the jitter, and that low fps alone doesn't (see its
  docs/re-notes.md 2026-10-24). Fix idea: track the live zero pose
  (`IVRSystem::GetRawZeroPoseToStandingAbsoluteTrackingPose`), store raw
  orientations and apply the transform at match time, drop `m_correction`.
  Warn when the best match isn't near exact.
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
