{
  description = "Give the Logitech MX Keys F4 key back on macOS without giving up F12";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
      ];
      forEach = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forEach (pkgs: rec {
        mxkeys-f4 = pkgs.callPackage ./nix/package.nix { };
        default = mxkeys-f4;
      });

      apps = forEach (pkgs: rec {
        mxkeys-f4 = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/mxkeys-f4d";
        };
        default = mxkeys-f4;
      });

      # A Home Manager module rather than a nix-darwin one: the agent runs as the
      # user, is signed against the login keychain, and nothing it does needs
      # root.
      homeManagerModules.mxkeys-f4 = import ./nix/home-manager.nix { inherit self; };
      homeManagerModules.default = self.homeManagerModules.mxkeys-f4;

      devShells = forEach (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            nixfmt-rfc-style
          ];
        };
      });

      formatter = forEach (pkgs: pkgs.nixfmt-tree);
    };
}
