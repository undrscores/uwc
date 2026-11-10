{
  description = "uwc: a Wayland compositor with an infinite 2D canvas (Rust, smithay)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        uwc = pkgs.callPackage ./packaging/nix/package.nix { };
        default = uwc;
      });

      overlays.default = final: _prev: { uwc = final.callPackage ./packaging/nix/package.nix { }; };

      # programs.uwc.enable = true;  (a login session, portals, graphics)
      nixosModules.default = import ./packaging/nix/module.nix self;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.uwc ];
          packages = with pkgs; [ rustc cargo clippy rustfmt rust-analyzer xvfb-run xdotool imagemagick (python3.withPackages (p: [ p.pillow ])) ];
          # smithay loads libEGL / libGLESv2 at run time.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libglvnd pkgs.wayland pkgs.libxkbcommon ];
        };
      });
    };
}
