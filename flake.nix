{
  description = "GLLMH, a Linux system-user harness for humans, agents and services";

  inputs.nixpkgs.url = "tarball+https://channels.nixos.org/nixos-26.05/nixexprs.tar.zst";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      acceptance = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          self.nixosModules.acceptance
          {
            system.stateVersion = "26.05";
            boot.loader.grub.devices = [ "nodev" ];
            fileSystems."/" = {
              device = "/dev/disk/by-label/nixos";
              fsType = "ext4";
            };
          }
        ];
      };
    in
    {
      packages.${system} = {
        default = pkgs.pkgsStatic.callPackage ./nix/package.nix { };
        acceptance-services = pkgs.pkgsStatic.callPackage ./nix/package.nix { kind = "acceptance-services"; };
      };

      nixosModules = {
        gllmh = import ./nix/module.nix;
        acceptance = {
          imports = [ self.nixosModules.gllmh ./nix/acceptance.nix ];
        };
      };

      checks.${system} = {
        cargo-test = pkgs.callPackage ./nix/package.nix { kind = "tests"; };
        module-eval = pkgs.callPackage ./nix/module-check.nix { inherit acceptance; };
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc clippy rustfmt sqlite ];
      };
    };
}
