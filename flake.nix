# Drive's packages for Nix: the agent (`drive-agent`) and the node
# (`drive-node`), built with crane on the same pins as devenv.yaml (nixpkgs,
# rust-overlay) and the same Rust as rust-toolchain.toml (1.99.1).
#
#   nix build .#drive           # the agent: Linux with the FUSE mount, macOS without
#   nix build .#drive-node
#
# The program is becoming `drive` (`drive serve`, `drive mount`, …), with
# `drive-agent` kept as an alias. Until the crate is renamed the package
# installs bin/drive-agent plus bin/drive pointing at it, so services can
# call bin/drive now and keep working after the rename. `drive-agent` is an
# alias attribute of `drive`.
#
# Development stays in devenv (devenv.nix); this flake only packages.
#
# The web app (apps/drive) is not part of these builds: it depends on
# app-shell, a private repository, which a builder without GitHub
# credentials cannot fetch. The build source leaves apps/ out and the
# dependencies are vendored from a lockfile without app-shell's entry, so
# nothing private is ever fetched; cargo drops the entries only the app used
# (the build runs offline, without --locked).
{
  description = "Drive: the agent and the node";

  inputs = {
    # The pins in devenv.yaml.
    nixpkgs.url = "github:NixOS/nixpkgs/8ce4ef6cb6f871616146b9fe26d2a5ae594e94fe";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/5b9f1198b59bcb31c5c3250b332e7d77590e6306";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Nixpkgs 26.11 dropped x86_64-darwin (the Mac Pro); 26.05 is its last
    # release. A consumer with its own 26.05 darwin pin makes this follow it.
    nixpkgs-x86_64-darwin.url = "github:NixOS/nixpkgs/36fd15bbb657815e2028e31d715c2e43a1230c91";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-x86_64-darwin,
      rust-overlay,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f system);

      perSystem =
        system:
        let
          pkgs = import (if system == "x86_64-darwin" then nixpkgs-x86_64-darwin else nixpkgs) {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          inherit (pkgs) lib;
          isLinux = pkgs.stdenv.hostPlatform.isLinux;

          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain (_: toolchain);

          # The lockfile without the web app's private dependency, for
          # vendoring only: Cargo.lock itself stays as it is, and cargo drops
          # the entries only the web app used (offline, every source
          # vendored, no --locked).
          lock = lib.importTOML ./Cargo.lock;
          privateSources = [ "git+https://github.com/FastTrackStudios/app-shell" ];
          isPrivate = p: p ? source && lib.any (s: lib.hasPrefix s p.source) privateSources;
          prunedLock = lock // {
            package = lib.filter (p: !(isPrivate p) && p.name != "drive-app") lock.package;
          };

          # Rust sources plus the assets drive-sites embeds; no apps/ (the
          # build drops it from the workspace's members), no target/. A plain path, not a derivation, so evaluating the
          # package for another platform (the Mac Pro, from Linux) needs no
          # build (no import-from-derivation).
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./rust-toolchain.toml
              ./crates
              ./assets
            ];
          };

          # The FUSE mount is Linux's; on macOS the agent builds without it
          # (`serve` needs none of it).
          agentFeatures = if isLinux then "" else "--no-default-features";

          common = {
            inherit src;
            pname = "drive";
            version = (lib.importTOML ./Cargo.toml).workspace.package.version or "0.1.0";
            strictDeps = true;
            cargoVendorDir = craneLib.vendorCargoDeps {
              cargoLockParsed = prunedLock;
              # pdfcraft's repository commits Cargo.toml.orig files, a name
              # `cargo package` (which crane vendors git crates with)
              # refuses, and its crates include_bytes! fonts from the
              # repository's assets/, outside the crate, which a vendored
              # crate does not have: copy those into the crate.
              overrideVendorGitCheckout =
                _: drv:
                drv.overrideAttrs (old: {
                  nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.python3 ];
                  postPatch = ''
                    find . -name Cargo.toml.orig -delete
                    python3 ${./nix/inline-repo-includes.py}
                  '';
                });
            };
            # The lockfile loses entries only the web app used: let cargo
            # prune them, offline (every source is vendored).
            cargoExtraArgs = "--offline";
            # The workspace without the web app (left out of src).
            postPatch = ''
              sed -i -e 's#,[[:space:]]*"apps/\*"##' -e 's#"apps/\*",\{0,1\}##' Cargo.toml
              if grep -q '"apps/\*"' Cargo.toml; then
                echo "flake.nix: could not drop apps/* from the workspace members" >&2
                exit 1
              fi
            '';
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
              pkgs.perl
            ];
            buildInputs = lib.optionals isLinux [ pkgs.fuse3 ];
            doCheck = false;
          };

          # One dependency build for both binaries.
          cargoArtifacts = craneLib.buildDepsOnly (
            common
            // {
              cargoExtraArgs = "--offline -p drive-agent -p drive-node ${agentFeatures}";
            }
          );

          bin =
            name: extra: extra':
            craneLib.buildPackage (
              common
              // {
                inherit cargoArtifacts;
                pname = name;
                cargoExtraArgs = "--offline -p ${name} ${extra}";
                meta.mainProgram = name;
              }
              // extra'
            );
        in
        {
          # bin/drive, whatever the binary is called (the rename makes it
          # bin/drive itself; then bin/drive-agent is the alias).
          drive = bin "drive-agent" agentFeatures {
            postInstall = ''
              if [ ! -e $out/bin/drive ]; then ln -s drive-agent $out/bin/drive; fi
              if [ ! -e $out/bin/drive-agent ]; then ln -s drive $out/bin/drive-agent; fi
            '';
            meta.mainProgram = "drive";
          };
          drive-node = bin "drive-node" "" { };
          # dx matching the dioxus crate: builds the desktop app.
          dx = pkgs.callPackage ./nix/dx.nix { };
        }
        // lib.optionalAttrs isLinux {
          # The desktop app (apps/drive, feature `desktop`): Drive in the
          # system tray, its window a click away, built by dx (`dx build
          # --platform linux --renderer native`): dx fills in the asset!()
          # table the binary looks its CSS and icons up in, which a plain
          # cargo build leaves as placeholders. Its own source and lockfile
          # (the whole workspace, app-shell included: public since
          # 2026-11-24), so the agent's and the node's store paths do not
          # move with it. macOS gets Drive.app from the release instead
          # (macos/bundle-app.sh: dx bundle plus the File Provider extension).
          drive-desktop =
            let
              desktopSrc = lib.fileset.toSource {
                root = ./.;
                fileset = lib.fileset.unions [
                  ./Cargo.toml
                  ./Cargo.lock
                  ./rust-toolchain.toml
                  ./crates
                  ./assets
                  ./apps/drive
                ];
              };
              runtimeLibs = [
                pkgs.wayland
                pkgs.libxkbcommon
                pkgs.vulkan-loader
                pkgs.libGL
                pkgs.fontconfig
                pkgs.freetype
              ];
              args = {
                src = desktopSrc;
                pname = "drive-desktop";
                version = common.version;
                strictDeps = true;
                cargoVendorDir = craneLib.vendorCargoDeps {
                  cargoLock = ./Cargo.lock;
                  overrideVendorGitCheckout =
                    _: drv:
                    drv.overrideAttrs (old: {
                      nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.python3 ];
                      postPatch = ''
                        find . -name Cargo.toml.orig -delete
                        python3 ${./nix/inline-repo-includes.py}
                      '';
                    });
                };
                cargoExtraArgs = "--locked -p drive-app --no-default-features --features desktop";
                nativeBuildInputs = [
                  pkgs.pkg-config
                  pkgs.cmake
                  pkgs.perl
                  pkgs.makeWrapper
                  pkgs.wrapGAppsHook3
                  # stylo's build script (Blitz's style engine).
                  pkgs.python3
                ];
                buildInputs = [
                  pkgs.gtk3
                  pkgs.webkitgtk_4_1
                  pkgs.libsoup_3
                  pkgs.glib
                  pkgs.openssl
                  pkgs.fuse3
                ]
                ++ runtimeLibs;
                doCheck = false;
              };
              dx = pkgs.callPackage ./nix/dx.nix { };
            in
            craneLib.mkCargoDerivation (
              args
              // {
                cargoArtifacts = craneLib.buildDepsOnly args;
                nativeBuildInputs = args.nativeBuildInputs ++ [ dx ];
                dontWrapGApps = true;
                # dx keeps its own state under HOME.
                buildPhaseCargoCommand = ''
                  export HOME=$(mktemp -d)
                  (cd apps/drive && dx build --release --locked --platform linux --renderer native \
                    --no-default-features --features desktop)
                '';
                # dx's app folder: the program and assets/ beside it, where
                # the resolver looks (the program's own folder).
                installPhaseCommand = ''
                  app=target/dx/drive-app/release/linux/app
                  test -x $app/drive-app
                  mkdir -p $out/lib
                  cp -r $app $out/lib/drive-desktop
                  install -Dm444 /dev/stdin $out/share/applications/app.fasttrackstudio.drive.desktop <<DESKTOP
                  [Desktop Entry]
                  Type=Application
                  Name=Drive
                  Comment=Your files on every computer
                  Exec=drive-desktop
                  Icon=app.fasttrackstudio.drive
                  Categories=Utility;FileTools;
                  StartupWMClass=app.fasttrackstudio.drive
                  DESKTOP
                  sed -i 's/^[[:space:]]*//' $out/share/applications/app.fasttrackstudio.drive.desktop
                  for n in 32 128 256 512; do
                    install -Dm444 apps/drive/icons/''${n}x''${n}.png \
                      $out/share/icons/hicolor/''${n}x''${n}/apps/app.fasttrackstudio.drive.png
                  done
                '';
                # gappsWrapperArgs is filled in at fixup: wrap after it.
                postFixup = ''
                  makeWrapper $out/lib/drive-desktop/drive-app $out/bin/drive-desktop \
                    "''${gappsWrapperArgs[@]}" \
                    --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs}
                '';
                meta.mainProgram = "drive-desktop";
              }
            );
        };
    in
    {
      packages = forAll (
        system:
        let
          p = perSystem system;
        in
        p
        // {
          drive-agent = p.drive;
          default = p.drive;
        }
      );
    };
}
