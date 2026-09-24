{
  description = "Build a cargo project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };
  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      fenix,
      advisory-db,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib;

        rustToolchain = fenix.packages.${system}.latest.withComponents [
          "cargo"
          "clippy"
          "rust-analyzer"
          "rustc"
          "rustfmt"
          "rust-src"
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        unfilteredRoot = ./.;

        src = lib.fileset.toSource {
          root = unfilteredRoot;
          fileset = lib.fileset.unions [
            (craneLib.fileset.commonCargoSources unfilteredRoot)
            ./shaders
          ];
        };

        commonArgs = {
          inherit src;
          pname = "gilia";
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          cargoExtraArgs = "--features bin";
          strictDeps = true;

          nativeBuildInputs = [
            pkgs.clang
            pkgs.libclang.lib
            pkgs.pkg-config
          ];

          buildInputs = [
            pkgs.alsa-lib
            pkgs.ffmpeg-full
            pkgs.libass
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxi
            pkgs.libxkbcommon
            pkgs.libxrandr
            pkgs.mesa
            pkgs.pipewire
            pkgs.vulkan-loader
            pkgs.wayland
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.alsa-lib
            pkgs.ffmpeg-full
            pkgs.libass
            pkgs.libx11
            pkgs.libxcursor
            pkgs.libxi
            pkgs.libxkbcommon
            pkgs.libxrandr
            pkgs.mesa
            pkgs.pipewire
            pkgs.vulkan-loader
            pkgs.wayland
          ];
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        gilia = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );
      in
      {
        checks = {
          inherit gilia;

          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          fmt = craneLib.cargoFmt (
            commonArgs
            // {
              inherit src;
            }
          );

          toml-fmt = craneLib.taploFmt (
            commonArgs
            // {
              src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
            }
          );

          audit = craneLib.cargoAudit (
            commonArgs
            // {
              inherit src advisory-db;
            }
          );

          deny = craneLib.cargoDeny (
            commonArgs
            // {
              inherit src;
            }
          );

          nextest = craneLib.cargoNextest (
            commonArgs
            // {
              inherit cargoArtifacts;
              partitions = 1;
              partitionType = "count";
              cargoNextestPartitionsExtraArgs = "--no-tests=pass";
            }
          );
        };

        packages = {
          default = gilia;
        };

        apps.default = flake-utils.lib.mkApp {
          drv = gilia;
        };

        formatter = pkgs.alejandra;

        devShells.default = craneLib.devShell (
          commonArgs
          // {
            checks = self.checks.${system};

            version = null;

            packages = [ ];
          }
        );
      }
    );
}
