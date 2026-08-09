{
  pkgs,
  lib,
  nixosSystem,
}: let
  guest = nixosSystem.pkgs.stdenv.hostPlatform;
  system = guest.qemuArch;
  microvm = nixosSystem.config.microvm;
  inherit (import ./spindle-qemu-runner.nix {inherit lib;}) mkQemuRunner;
  baseConfigHash = lib.pipe nixosSystem.config.system.build.toplevel.outPath [
    (lib.strings.removePrefix "/nix/store/")
    (lib.strings.splitString "-")
    lib.head
  ];
  imageSpecJSON = pkgs.writeText "spec.json" (
    builtins.toJSON {
      arch = system;
      # earlyprintk is x86-only
      bootArgs = "${lib.optionalString guest.isx86_64 "earlyprintk=ttyS0 "}console=hvc0 reboot=t panic=-1 ${lib.concatStringsSep " " microvm.kernelParams}";
      kernel = "kernel";
      initrd = "initrd";
      runnerType = "qemu";
      # the runner has to boot the machine the guest was built for
      runnerConfig = mkQemuRunner {
        arch = system;
        machine = microvm.qemu.machine;
      };
      memoryMiB = microvm.mem;
      storeDisk = "store-disk";
      storeDiskType = microvm.storeDiskType;
      vcpus = microvm.vcpu;
      shell = "/run/current-system/sw/bin/bash";
      baseConfigHash = baseConfigHash;
      networkInterfaces =
        map (interface: {
          type = "slirp4netns";
          id = interface.id;
          mac = interface.mac;
        })
        microvm.interfaces;
      volumes =
        map (volume: {
          fsType = volume.fsType;
          image = volume.image;
          imageType = volume.imageType;
          mountPoint = volume.mountPoint;
          readOnly = volume.readOnly;
          sizeMiB = volume.size;
        })
        microvm.volumes;
    }
  );
in
  pkgs.runCommand "spindle-nixos-image-${system}" {} ''
    mkdir -p "$out"
    cp ${imageSpecJSON} "$out/spec.json"
    ln -s ${microvm.kernel}/${guest.linux-kernel.target} "$out/kernel"
    ln -s ${microvm.initrdPath} "$out/initrd"
    ln -s ${microvm.storeDisk} "$out/store-disk"
  ''
