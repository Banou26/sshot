{ pkgs }:

pkgs.rustPlatform.buildRustPackage {
  pname = "sshot";
  version = "0.1.0";
  src = ./.;

  cargoHash = "";

  nativeBuildInputs = with pkgs; [ pkg-config ];
  buildInputs = with pkgs; [
    wayland
    wayland-protocols
    libxkbcommon
  ];

  postInstall = ''
    wrapProgram $out/bin/screenshot \
      --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.wl-clipboard ]}
  '';

  meta.mainProgram = "sshot";
}
