{
  self,
  microvm,
}: runnerModule: {
  imports = [
    microvm.nixosModules.microvm
    ./base.nix
    runnerModule
    self.nixosModules.shuttle
    ({pkgs, ...}: {
      services.tangled.shuttle.enable = true;

      environment.etc = {
        "spindle/nixos/base.nix".source = ./base.nix;
        "spindle/nixos/runner.nix".source = runnerModule;
        "spindle/nixos/shuttle.nix".text = ''
          { config, lib, pkgs, ... }:
          {
            imports = [${../modules/shuttle.nix}];
            services.tangled.shuttle.package = lib.mkDefault ${self.packages.${pkgs.stdenv.hostPlatform.system}.shuttle};
          }
        '';
        "spindle/nixos/user-config.nix".source = ./user-config.nix;
        "spindle/nixos/microvm".source = microvm;
        # pkgs.path is fine here because we pass the nixpkgs source into the vm in ./base.nix
        "spindle/nixos/default.nix".text = ''
          let
            nixpkgs = ${pkgs.path};
            nixos = import (nixpkgs + "/nixos") {
              system = "${pkgs.stdenv.hostPlatform.system}";
              configuration = {
                imports = [
                  /etc/spindle/nixos/microvm/nixos-modules/microvm/default.nix
                  /etc/spindle/nixos/base.nix
                  /etc/spindle/nixos/runner.nix
                  /etc/spindle/nixos/shuttle.nix
                  /etc/spindle/nixos/user-config.nix
                ];
                services.tangled.shuttle.enable = true;
              };
            };
          in
          nixos.system
        '';
      };
    })
  ];
}
