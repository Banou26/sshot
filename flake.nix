{
  description = "sshot – Wayland screenshot tool for KDE Plasma";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        runtimeDeps = with pkgs; [
          wl-clipboard   # wl-copy for clipboard
          kdotool        # active window name on KDE Wayland
        ];

        nativeBuildDeps = with pkgs; [
          pkg-config
          rustPlatform.bindgenHook
        ];

        buildDeps = with pkgs; [
          dbus
          wayland
          libxkbcommon
        ];
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "sshot";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = nativeBuildDeps ++ [ pkgs.makeWrapper ];
          buildInputs = buildDeps;

          postInstall = ''
            wrapProgram $out/bin/sshot \
              --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = buildDeps ++ runtimeDeps ++ nativeBuildDeps ++ [
            (pkgs.rust-bin.stable.latest.default.override {
              extensions = [ "rust-src" "rust-analyzer" ];
            })
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildDeps;
        };
      });
}
