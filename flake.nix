{
  description = "forge — a session-aware GTK4 terminal with structured command blocks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
      ]
      (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          manifest = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          appId = "io.github.beamiter.forge";

          package = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = self;

            cargoLock = {
              lockFile = ./Cargo.lock;
              # Git dependencies are not covered by Cargo.lock checksums, so
              # Nix needs an explicit hash per revision. Update these whenever
              # the jagent / jterm_core pins in Cargo.lock change.
              outputHashes = {
                # 6ed0b9f037eff9359d62ed448e0d117327fabe26
                "jagent-0.7.0" = "sha256-j5WkB+u+/IqNb4UAKvUwTkalI9KDjWZ2M4omGLsgIb4=";
                # 1ef0212d0e80a269c9b9a30ba2b179d6f570bb92
                "jterm_core-0.2.0" = "sha256-wiORuGS8EudsOGXZyxO672SB+tMCEjSmublf2qPtEFs=";
              };
            };
            strictDeps = true;

            nativeBuildInputs = with pkgs; [
              pkg-config
              wrapGAppsHook4
            ];

            buildInputs = with pkgs; [
              gtk4
              libadwaita
              vte-gtk4
              pcre2
              fcitx5-gtk
            ];

            FCITX5_GTK_PATH = "${pkgs.fcitx5-gtk}/lib/gtk-4.0";

            # Display-backed GTK regressions run in a dedicated Xvfb CI job.
            # Running the suite in the Nix build sandbox would still fail for
            # lack of a real display server.
            doCheck = false;

            postInstall = ''
              install -Dm644 data/${appId}.desktop \
                "$out/share/applications/${appId}.desktop"
              install -Dm644 data/${appId}.metainfo.xml \
                "$out/share/metainfo/${appId}.metainfo.xml"
              install -Dm644 data/${appId}.svg \
                "$out/share/icons/hicolor/scalable/apps/${appId}.svg"
              install -Dm644 data/${appId}-128.png \
                "$out/share/icons/hicolor/128x128/apps/${appId}.png"
              install -Dm644 data/${appId}-256.png \
                "$out/share/icons/hicolor/256x256/apps/${appId}.png"
              install -Dm644 config.toml.example \
                "$out/share/doc/forge/config.toml.example"
              install -Dm644 README.md "$out/share/doc/forge/README.md"
              install -Dm644 Cargo.lock "$out/share/doc/forge/Cargo.lock"
              install -Dm755 scripts/support-bundle.sh \
                "$out/bin/forge-support-bundle"

              install -d "$out/share/forge/shell-integration"
              install -m644 scripts/shell-integration/README.md \
                scripts/shell-integration/forge.* \
                "$out/share/forge/shell-integration/"
              bash scripts/install-workflow-assets.sh scripts/workflows \
                "$out/share/forge/workflows"
              install -Dm644 scripts/notebooks/welcome.jtnb.md \
                "$out/share/forge/notebooks/welcome.jtnb.md"
            '';

            preFixup = ''
              gappsWrapperArgs+=(
                --set-default FORGE_WORKFLOW_DIR "$out/share/forge/workflows"
                --set-default FORGE_ASSET_DIR "$out/share/forge"
              )
            '';

            meta = with pkgs.lib; {
              description = manifest.package.description;
              homepage = manifest.package.repository;
              mainProgram = "forge";
              platforms = platforms.linux;
            };
          };
        in
        {
          packages.default = package;
          apps.default = (flake-utils.lib.mkApp { drv = package; }) // {
            inherit (package) meta;
          };
          checks.package = package;
          formatter = pkgs.nixfmt;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ package ];
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              cargo-audit
              cargo-deny
              cargo-watch
              shellcheck
              dbus
              xorg-server
              xauth
              xvfb-run

              gtk4
              glib
              pkg-config
              libadwaita
              vte
              vte-gtk4
              pcre2
              fcitx5-gtk

              glade
              cambalache
              xdotool
              jq
              valgrind
              strace
              patchelf
              fuse
              fakeroot
              openssl
            ];

            shellHook = ''
              export GSETTINGS_SCHEMA_DIR="${pkgs.gtk4}/share/gsettings-schemas/:${pkgs.glib}/share/gsettings-schemas/"
              export RUST_BACKTRACE=1
              export GTK_IM_MODULE="''${GTK_IM_MODULE:-fcitx}"
              export XMODIFIERS="''${XMODIFIERS:-@im=fcitx}"
              export QT_IM_MODULE="''${QT_IM_MODULE:-fcitx}"
              export GTK_PATH="${pkgs.fcitx5-gtk}/lib/gtk-4.0''${GTK_PATH:+:$GTK_PATH}"
              export FCITX5_GTK_PATH="${pkgs.fcitx5-gtk}/lib/gtk-4.0"
              echo "forge development environment ready. Run 'make verify'."
            '';
          };
        }
      );
}
