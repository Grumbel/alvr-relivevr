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
        cargoDeps = pkgs.rustPlatform.importCargoLock {
          lockFile = ./Cargo.lock;
          outputHashes = {
            "settings-schema-0.2.0" = "sha256-luEdAKDTq76dMeo5kA+QDTHpRMFUg3n0qvyQ7DkId0k=";
          };
        };
      });
    in
    {
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
