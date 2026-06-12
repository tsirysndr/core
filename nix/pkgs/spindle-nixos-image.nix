{
  pkgs,
  lib,
  nixosSystem,
}: let
  system = nixosSystem.pkgs.stdenv.hostPlatform.qemuArch;
  microvm = nixosSystem.config.microvm;
  baseConfigHash = lib.pipe nixosSystem.config.system.build.toplevel.outPath [
    (lib.strings.removePrefix "/nix/store/")
    (lib.strings.splitString "-")
    lib.head
  ];
  imageSpecJSON = pkgs.writeText "spec.json" (
    builtins.toJSON {
      arch = system;
      bootArgs = "earlyprintk=ttyS0 console=hvc0 reboot=t panic=-1 ${lib.concatStringsSep " " microvm.kernelParams}";
      kernel = "kernel";
      initrd = "initrd";
      runnerType = "qemu";
      runnerConfig = {
        cpu = "host,+x2apic,-sgx";
        machine = "microvm,accel=kvm:tcg,acpi=on,mem-merge=on,pcie=off,pic=off,pit=off,rtc=on,usb=off";
        console = "hvc0";
        extraArgs = [];
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
    ln -s ${microvm.kernel}/bzImage "$out/kernel"
    ln -s ${microvm.initrdPath} "$out/initrd"
    ln -s ${microvm.storeDisk} "$out/store-disk"
  ''
