{
  description = "jterm4 — a session-aware GTK4 terminal with structured command blocks";

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
          appId = "io.github.beamiter.jterm4";

          package = pkgs.rustPlatform.buildRustPackage {
            pname = manifest.package.name;
            version = manifest.package.version;
            src = self;

            cargoLock.lockFile = ./Cargo.lock;
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

            # GTK behavior is tested under a real display in CI. Running the
            # suite in the Nix build sandbox would produce display failures.
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
                "$out/share/doc/jterm4/config.toml.example"
              install -Dm644 README.md "$out/share/doc/jterm4/README.md"
              install -Dm644 Cargo.lock "$out/share/doc/jterm4/Cargo.lock"
              install -Dm755 scripts/support-bundle.sh \
                "$out/bin/jterm4-support-bundle"

              install -d "$out/share/jterm4/shell-integration"
              install -m644 scripts/shell-integration/README.md \
                scripts/shell-integration/jterm4.* \
                "$out/share/jterm4/shell-integration/"
              install -d "$out/share/jterm4/workflows"
              install -m644 scripts/workflows/*.yaml \
                "$out/share/jterm4/workflows/"
              install -Dm644 scripts/notebooks/welcome.jtnb.md \
                "$out/share/jterm4/notebooks/welcome.jtnb.md"
            '';

            preFixup = ''
              gappsWrapperArgs+=(
                --set-default JTERM4_WORKFLOW_DIR "$out/share/jterm4/workflows"
                --set-default JTERM4_ASSET_DIR "$out/share/jterm4"
              )
            '';

            meta = with pkgs.lib; {
              description = manifest.package.description;
              homepage = manifest.package.repository;
              mainProgram = "jterm4";
              platforms = platforms.linux;
            };
          };
        in
        {
          packages.default = package;
          apps.default = flake-utils.lib.mkApp { drv = package; };
          checks.package = package;
          formatter = pkgs.nixfmt-rfc-style;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ package ];
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              cargo-audit
              cargo-watch
              shellcheck

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
              echo "jterm4 development environment ready. Run 'make verify'."
            '';
          };
        }
      );
}
