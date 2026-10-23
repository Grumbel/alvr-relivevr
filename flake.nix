{
  description = "ALVR fork streaming to the AMD ReliveVR (Wireless GVR) headset client";

  inputs = {
    # Must match the NixOS system that runs Steam: SteamVR's runtime uses the
    # host glibc, so a driver built against a newer glibc fails to load
    # ("GLIBC_2.43 not found"). Update together with the system.
    nixpkgs.url = "github:NixOS/nixpkgs/7993f34ee47e28a3d607a525aadc6c94b73be728";
    # openvr is a git submodule
    self.submodules = true;
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;

      # ALVR master builds against FFmpeg 8.1 with its own VAAPI patches
      # (alvr/xtask/patches); nixpkgs' ffmpeg-alvr is 6.0 for ALVR 20.14.1.
      ffmpeg-alvr = (pkgs.ffmpeg_8.override {
        withDocumentation = false;
        withHtmlDoc = false;
        withManPages = false;
        withPodDoc = false;
        withTxtDoc = false;
      }).overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ map (p: ./alvr/xtask/patches + "/${p}")
          (builtins.attrNames (builtins.readDir ./alvr/xtask/patches));
        doCheck = false;
      });

      # Library locations for alvr/server_openvr/build.rs (instead of the
      # `cargo xtask prepare-deps` downloads).
      libEnv = {
        ALVR_FFMPEG_DIR = lib.getDev ffmpeg-alvr;
        ALVR_X264_DIR = lib.getDev pkgs.x264;
        ALVR_VULKAN_HEADERS_DIR = "${pkgs.vulkan-headers}/include";
      };

      # Build recipe (deps, postInstall) from nixpkgs' alvr package; source,
      # FFmpeg and vendored crates are ours.
      alvr = (pkgs.alvr.override { inherit ffmpeg-alvr; }).overrideAttrs (old: {
        version = "master-relivevr";
        src = self;
        patches = [ ];
        env = old.env // libEnv;
        # ALVR master's streamer tree is bin/ + lib64/alvr (driver) + libexec/,
        # without the share/ of 20.14.1. The dashboard finds the driver at
        # ../lib64/alvr relative to itself.
        postInstall = ''
          cp -rf ./build/alvr_streamer_linux/. $out/
          install -Dm644 alvr/xtask/resources/alvr.desktop $out/share/applications/alvr.desktop
          install -Dm644 resources/ALVR-Icon.svg $out/share/icons/hicolor/scalable/apps/alvr.svg
        '';
        cargoDeps = pkgs.rustPlatform.importCargoLock {
          lockFile = ./Cargo.lock;
          outputHashes = {
            "settings-schema-0.2.0" = "sha256-luEdAKDTq76dMeo5kA+QDTHpRMFUg3n0qvyQ7DkId0k=";
          };
        };
      });

      app = name: description: runtimeInputs: text: {
        type = "app";
        meta.description = description;
        program = "${pkgs.writeShellApplication { inherit name runtimeInputs text; }}/bin/${name}";
      };

      # Point SteamVR at a driver directory and drop every other ALVR driver,
      # as the dashboard does on "Launch SteamVR" (two ALVR drivers would fight
      # over the same sockets).
      mkRegisterDriver = name: description: defaultDir: app name description [ pkgs.jq pkgs.coreutils ] ''
          driver=$(realpath "''${1:-${defaultDir}}")
          if [ ! -f "$driver/driver.vrdrivermanifest" ]; then
            echo "error: $driver has no driver.vrdrivermanifest" >&2
            exit 1
          fi
          paths="''${XDG_CONFIG_HOME:-$HOME/.config}/openvr/openvrpaths.vrpath"
          if [ ! -f "$paths" ]; then
            echo "error: $paths not found; start SteamVR once first" >&2
            exit 1
          fi
          tmp=$(mktemp "$paths.XXXXXX")
          jq --arg d "$driver" '
            .external_drivers = ([(.external_drivers // [])[]
                                  | select(ascii_downcase | contains("alvr") | not)]
                                 + [$d])' "$paths" > "$tmp"
          mv "$tmp" "$paths"
          echo "SteamVR driver: $driver"
        '';

      registerDriver = mkRegisterDriver "alvr-relivevr-register-driver"
        "Register this flake's ALVR ReliveVR driver with SteamVR" "${alvr}/lib64/alvr";
    in
    {
      apps.${system} = rec {
        default = dashboard;

        # ALVR dashboard: settings, logs, "Launch SteamVR" (registers the driver).
        # LD_LIBRARY_PATH must not leak into SteamVR's runtime.
        dashboard = app "alvr-relivevr-dashboard" "ALVR dashboard (ReliveVR fork)" [ ] ''
          unset LD_LIBRARY_PATH
          exec ${alvr}/bin/alvr_dashboard "$@"
        '';

        register-driver = registerDriver;

        # Driver from `nix develop -c cargo xtask build-streamer --release`
        # (run from the repository root).
        register-dev-driver = mkRegisterDriver "alvr-relivevr-register-dev-driver"
          "Register the driver in ./build/alvr_streamer_linux with SteamVR"
          "build/alvr_streamer_linux/lib64/alvr";

        # Register the driver and start SteamVR through the system's Steam.
        steamvr = app "alvr-relivevr-steamvr" "Register the driver and launch SteamVR" [ ] ''
          ${registerDriver.program}
          unset LD_LIBRARY_PATH
          exec steam steam://run/250820
        '';

        # Restart the ReliveVR client on the headset so it reconnects.
        restart-client = app "relivevr-restart-client"
          "Restart the ReliveVR app on the headset (adb)" [ pkgs.android-tools ] ''
            adb shell am force-stop com.amd.wirelessgvr
            sleep 1
            adb shell am start -n com.amd.wirelessgvr/.MainActivity
          '';

        # Client log (decoder health, pose lookups, configured FOV/codec).
        logcat = app "relivevr-logcat"
          "Follow the ReliveVR client log (adb logcat, AMF_TRACE)" [ pkgs.android-tools ] ''
            adb logcat -c
            adb logcat -v time | grep --line-buffered AMF_TRACE
          '';
      };

      packages.${system} = {
        default = alvr;
        inherit ffmpeg-alvr;
      };

      devShells.${system}.default = pkgs.mkShell ({
        inputsFrom = [ alvr ];
        nativeBuildInputs = [ pkgs.rustfmt pkgs.clippy ];
        inherit (pkgs.alvr.drvAttrs) NIX_CFLAGS_COMPILE RUSTFLAGS;
      } // libEnv);
    };
}
