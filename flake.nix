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

      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.programs.sshot;
          pkg = self.packages.${pkgs.system}.default;
          configJson = builtins.toJSON {
            save = {
              directory = cfg.saveDirectory;
              subfolder = cfg.subfolder;
              window_format = cfg.windowFormat;
              region_format = cfg.regionFormat;
            };
            appearance = {
              dim_factor = cfg.dimFactor;
              border_width = cfg.borderWidth;
              window_border_color = cfg.windowBorderColor;
              region_border_color = cfg.regionBorderColor;
            };
            shortcut = cfg.shortcut;
          };
        in
        {
          options.programs.sshot = {
            enable = lib.mkEnableOption "sshot screenshot tool";

            shortcut = lib.mkOption {
              type = lib.types.str;
              default = "Print";
              description = "Global shortcut key (e.g. 'Print', 'Ctrl+Shift+4', 'Meta+Shift+S')";
            };

            saveDirectory = lib.mkOption {
              type = lib.types.str;
              default = "${config.home.homeDirectory}/Pictures/Screenshots";
              description = "Base directory for screenshots";
            };

            subfolder = lib.mkOption {
              type = lib.types.str;
              default = "%Y-%m";
              description = "Subfolder format (strftime)";
            };

            windowFormat = lib.mkOption {
              type = lib.types.str;
              default = "{title} %Y-%m-%d-%H-%M-%S-{random}";
              description = "Filename format for window captures. Variables: {title}, {random}, and strftime.";
            };

            regionFormat = lib.mkOption {
              type = lib.types.str;
              default = "%Y-%m-%d-%H-%M-%S-{random}";
              description = "Filename format for region captures. Variables: {random}, and strftime.";
            };

            dimFactor = lib.mkOption {
              type = lib.types.float;
              default = 0.75;
              description = "Dim factor for non-highlighted areas (0.0 = black, 1.0 = none)";
            };

            borderWidth = lib.mkOption {
              type = lib.types.int;
              default = 3;
              description = "Highlight border width in pixels";
            };

            windowBorderColor = lib.mkOption {
              type = lib.types.listOf lib.types.int;
              default = [ 80 140 255 ];
              description = "Window highlight border color [R G B]";
            };

            regionBorderColor = lib.mkOption {
              type = lib.types.listOf lib.types.int;
              default = [ 255 255 255 ];
              description = "Region selection border color [R G B]";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ pkg ];

            xdg.configFile."sshot/config.json" = {
              text = configJson;
              force = true;
            };

            xdg.configFile."autostart/sshot.desktop".text = ''
              [Desktop Entry]
              Name=sshot
              Exec=sshot --daemon
              Type=Application
              X-KDE-autostart-phase=2
            '';
          };
        };
    };
}
