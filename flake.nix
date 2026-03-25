{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in
    {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "sshot";
        version = "0.1.0";
        src = ./.;

        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = with pkgs; [ pkg-config makeWrapper ];
        buildInputs = with pkgs; [ wayland wayland-protocols libxkbcommon ];

        postInstall = ''
          wrapProgram $out/bin/sshot \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.kdePackages.spectacle pkgs.wl-clipboard ]}
        '';

        meta.mainProgram = "sshot";
      };
    };
}
