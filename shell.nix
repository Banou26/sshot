{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    pkg-config
    cargo
    rustc
  ];
  buildInputs = with pkgs; [
    wayland
    wayland-protocols
    libxkbcommon
  ];
  # Runtime deps for testing
  packages = with pkgs; [
    wl-clipboard
  ];
}
