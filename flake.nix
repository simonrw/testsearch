{
  description = "Flake utils demo";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [
        ];

        pkgs = import nixpkgs {
          inherit overlays system;
        };
      in
      {
        packages.default = import ./default.nix { inherit pkgs; };
        packages.iterfzf = pkgs.python3Packages.iterfzf.overrideAttrs (p: {
              doInstallCheck = false;
            });
        devShells.default = import ./shell.nix { inherit pkgs; };
      }
    );
}
