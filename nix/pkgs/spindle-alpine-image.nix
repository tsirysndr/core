{
  pkgsStatic,
  lib,
  runCommand,
  writeText,
  squashfsTools,
  spindle-image-helpers,
  binutils,
  rootfs,
  kernel,
  initramfs,
  modloop,
  repositories,
  arch ? "x86_64",
}: let
  nix = pkgsStatic.nixStatic;
  bash = pkgsStatic.bashNonInteractive;
  curl = pkgsStatic.curlMinimal;
  jq = pkgsStatic.jq;
  git = pkgsStatic.callPackage ./spindle-static-git.nix {};
  # we don't include gnused, xxd etc. here because busybox has them
  # we want to keep the image this image small!
  guestTools = [nix bash git curl jq];

  # run by busybox at sysinit
  setupScript = writeText "spindle-setup" ''
    #!/bin/sh

    mountpoint -q /proc || mount -t proc proc /proc
    mountpoint -q /sys || mount -t sysfs sys /sys
    mountpoint -q /dev || mount -t devtmpfs dev /dev
    mountpoint -q /dev/pts || {
      install -d /dev/pts
      mount -t devpts devpts /dev/pts
    }
    mountpoint -q /dev/shm || {
      install -d /dev/shm
      mount -t tmpfs -o mode=1777 shm /dev/shm
    }
    mountpoint -q /run || mount -t tmpfs -o mode=0755 run /run
    mountpoint -q /tmp || mount -t tmpfs -o mode=1777 tmp /tmp

    modprobe vmw_vsock_virtio_transport
    # shuttle's cache enqueue listener binds a guest-local (CID 1) vsock
    modprobe vsock_loopback
    modprobe ext4

    # cgroup2 setup, normally we would do this with rc-service
    # but minirootfs does not ship with those so we set it up ourselves.
    mountpoint -q /sys/fs/cgroup || {
      install -d /sys/fs/cgroup
      mount -t cgroup2 -o nsdelegate cgroup2 /sys/fs/cgroup
      chown -R spindle-workflow:spindle-workflow /sys/fs/cgroup 2>/dev/null || true
    }

    # the initramfs mdev leaves these 0660, which breaks non-root workflows
    chmod 666 /dev/null /dev/zero /dev/full /dev/random /dev/urandom /dev/tty /dev/ptmx 2>/dev/null

    hostname -F /etc/hostname

    spindle-system-init
  '';

  inittab = writeText "inittab" ''
    ::sysinit:/sbin/spindle-setup
    ::respawn:env TMPDIR=/workspace/.nix/build /usr/local/bin/nix-daemon
    ::respawn:env NIX_REMOTE=daemon /usr/bin/shuttle
    ::ctrlaltdel:/sbin/reboot
  '';

  profileScript = writeText "spindle-profile" ''
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
    export GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt
    export NIX_REMOTE=daemon
  '';

  apkRepositories = writeText "apk-repositories" (builtins.concatStringsSep "\n" repositories + "\n");

  imageSpecJSON = writeText "spec.json" (
    builtins.toJSON {
      inherit arch;
      bootArgs = "earlyprintk=ttyS0 console=hvc0 reboot=t panic=-1 root=/dev/vda rootfstype=squashfs modules=virtio_blk,virtio_net,virtio_console overlaytmpfs=yes init=/sbin/init";
      kernel = "kernel";
      initrd = "initrd";
      runnerType = "qemu";
      runnerConfig = (import ./spindle-qemu-runner.nix {inherit lib;}).mkQemuRunner {inherit arch;};
      memoryMiB = 4096;
      storeDisk = "store-disk";
      storeDiskType = "squashfs";
      vcpus = 2;
      shell = "/usr/local/bin/bash";
      networkInterfaces = [
        {
          type = "slirp4netns";
          id = "net0";
          mac = "02:00:00:00:10:01";
        }
      ];
      volumes = [
        {
          fsType = "ext4";
          image = "workspace.img";
          imageType = "raw";
          mountPoint = "/workspace";
          readOnly = false;
          sizeMiB = 1024 * 16; # 16 GB
        }
      ];
    }
  );
in
  runCommand "spindle-alpine-image-${arch}" {
    nativeBuildInputs = [squashfsTools binutils];
  } ''
    mkdir -p rootfs
    tar -xzpf ${rootfs} -C rootfs

    # kernel modules from modloop (ships its own modules.dep, no depmod needed)
    unsquashfs -q -d modloop ${modloop}
    mkdir -p rootfs/lib/modules
    cp -a modloop/modules/* rootfs/lib/modules/

    ${spindle-image-helpers.setupRootfs} rootfs
    install -D -m 0755 ${setupScript} rootfs/sbin/spindle-setup
    install -D -m 0644 ${inittab} rootfs/etc/inittab
    install -D -m 0644 ${profileScript} rootfs/etc/profile.d/01-spindle.sh

    # install dependencies
    ${spindle-image-helpers.installGuestTools} rootfs ${toString guestTools}

    # scripts commonly hardcode #!/bin/bash
    ln -sf ${bash}/bin/bash rootfs/bin/bash

    install -D -m 0644 ${apkRepositories} rootfs/etc/apk/repositories

    mkdir -p "$out"
    mksquashfs rootfs "$out/store-disk" -comp zstd -Xcompression-level 19 -noappend -no-xattrs -all-root -quiet \
      -p '/sbin/apk m 4755 0 0' # suid apk so spindle-workflow can use it without having to doas or smth
    cp ${kernel} "$out/kernel"
    cp ${initramfs} "$out/initrd"
    cp ${imageSpecJSON} "$out/spec.json"
  ''
