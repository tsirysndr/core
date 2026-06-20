{
  pkgsStatic,
  runCommand,
  writeText,
  squashfsTools,
  shuttle,
  binutils,
  publicsuffix-list,
  rootfs,
  kernel,
  initramfs,
  modloop,
  repositories,
  arch ? "x86_64",
}: let
  nix = pkgsStatic.nixStatic;
  bash = pkgsStatic.bashNonInteractive;
  git =
    (pkgsStatic.gitMinimal.override {
      curl = pkgsStatic.curlMinimal;
      pythonSupport = false;
      withManual = false;
      nlsSupport = false;
    }).overrideAttrs (old: {
      doCheck = false;
      doInstallCheck = false;
      configureFlags = (old.configureFlags or []) ++ ["ac_cv_lib_curl_curl_global_init=yes"];
    });
  guestTools = [nix bash git];

  # run by busybox at sysinit
  setupScript = writeText "spindle-setup" ''
    #!/bin/sh

    mountpoint -q /proc || mount -t proc proc /proc
    mountpoint -q /sys || mount -t sysfs sys /sys
    mountpoint -q /dev || mount -t devtmpfs dev /dev
    mountpoint -q /dev/pts || {
      mkdir -p /dev/pts
      mount -t devpts devpts /dev/pts
    }
    mountpoint -q /dev/shm || {
      mkdir -p /dev/shm
      mount -t tmpfs -o mode=1777 shm /dev/shm
    }
    mountpoint -q /run || mount -t tmpfs -o mode=0755 run /run
    mountpoint -q /tmp || mount -t tmpfs -o mode=1777 tmp /tmp

    # the initramfs mdev leaves these 0660, which breaks non-root workflows
    chmod 666 /dev/null /dev/zero /dev/full /dev/random /dev/urandom /dev/tty /dev/ptmx 2>/dev/null

    modprobe vmw_vsock_virtio_transport
    # shuttle's cache enqueue listener binds a guest-local (CID 1) vsock
    modprobe vsock_loopback
    modprobe ext4

    # /dev/vda is the squashfs root; the first spindle volume backs /workspace
    if [ -b /dev/vdb ]; then
      mount -t ext4 /dev/vdb /workspace
      mkdir -p /workspace/repo
      chown spindle-workflow:spindle-workflow /workspace /workspace/repo
    fi

    ip link set lo up
    ip link set eth0 up
    ip addr add 10.0.3.15/24 dev eth0
    ip route add default via 10.0.3.2
    hostname -F /etc/hostname
  '';

  inittab = writeText "inittab" ''
    ::sysinit:/sbin/spindle-setup
    ::respawn:/usr/local/bin/nix-daemon
    ::respawn:env NIX_REMOTE=daemon /usr/bin/shuttle
    ::ctrlaltdel:/sbin/reboot
  '';

  profileScript = writeText "spindle-profile" ''
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
    export GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt
    export NIX_REMOTE=daemon
  '';

  # mirror nix/microvm/base.nix and nix/modules/shuttle.nix
  nixConf = writeText "nix.conf" ''
    experimental-features = nix-command flakes
    trusted-users = root
    allowed-users = spindle-workflow
    post-build-hook = /usr/libexec/spindle-post-build-hook
    !include /run/spindle/nix.conf
  '';

  apkRepositories = writeText "apk-repositories" (builtins.concatStringsSep "\n" repositories + "\n");

  postBuildHook = writeText "spindle-post-build-hook" ''
    #!/bin/sh
    set -f

    if [ -z "''${OUT_PATHS:-}" ]; then
      exit 0
    fi

    # OUT_PATHS is intentionally split into individual store paths
    exec /usr/bin/shuttle enqueue-built-paths $OUT_PATHS
  '';

  imageSpecJSON = writeText "spec.json" (
    builtins.toJSON {
      inherit arch;
      bootArgs = "earlyprintk=ttyS0 console=hvc0 reboot=t panic=-1 root=/dev/vda rootfstype=squashfs modules=virtio_blk,virtio_net,virtio_console overlaytmpfs=yes init=/sbin/init";
      kernel = "kernel";
      initrd = "initrd";
      runnerType = "qemu";
      runnerConfig = {
        cpu = "host,+x2apic,-sgx";
        machine = "microvm,accel=kvm:tcg,acpi=on,mem-merge=on,pcie=off,pic=off,pit=off,rtc=on,usb=off";
        console = "hvc0";
        extraArgs = [];
      };
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

    install -D -m 0755 ${shuttle}/bin/shuttle rootfs/usr/bin/shuttle
    install -D -m 0755 ${setupScript} rootfs/sbin/spindle-setup
    install -D -m 0644 ${inittab} rootfs/etc/inittab
    install -D -m 0644 ${profileScript} rootfs/etc/profile.d/01-spindle.sh
    install -D -m 0644 ${nixConf} rootfs/etc/nix/nix.conf
    install -D -m 0755 ${postBuildHook} rootfs/usr/libexec/spindle-post-build-hook

    # install dependencies
    # we only copy binaries + libexec for minimal deps so the image size doesn't
    # increase so much (if we copy the whole guestTools closure for example, it
    # doubles the disk size)
    mkdir -p rootfs/nix/store rootfs/usr/local/bin
    for pkg in ${toString guestTools}; do
      for bin in "$pkg/bin/"*; do
        [[ -e "$bin" ]] || continue
        name=$(basename "$bin")
        # we resolve symlinks as to copy the actual binaries
        if [[ -L "$bin" ]]; then
          real=$(readlink "$bin")
        else
          real="$bin"
        fi
        # handle symlinks properly
        if [[ "$real" != /nix/store* ]]; then
          ln -vsf "$real" "rootfs/usr/local/bin/$name"
        else
          cp -v "$real" "rootfs/usr/local/bin/$name"
        fi
      done
      # libexec has binaries used by packages even if statically compiled
      if [[ -d "$pkg/libexec" ]]; then
        mkdir -p "rootfs$pkg"
        cp -av "$pkg/libexec" "rootfs$pkg/"
      fi
    done
    # this is necessary for nix to work, it is not a library but nix hardcodes
    # it in it's binary
    cp -rv ${publicsuffix-list} rootfs/nix/store/

    # scripts commonly hardcode #!/bin/bash
    ln -sf ${bash}/bin/bash rootfs/bin/bash

    echo "spindle-microvm" > rootfs/etc/hostname
    printf 'nameserver 127.0.0.1\n' > rootfs/etc/resolv.conf
    install -D -m 0644 ${apkRepositories} rootfs/etc/apk/repositories

    echo "spindle-workflow:x:970:970:spindle workflow:/workspace:/bin/sh" >> rootfs/etc/passwd
    echo "spindle-workflow:x:970:" >> rootfs/etc/group
    echo "spindle-workflow:!::0:::::" >> rootfs/etc/shadow
    mkdir -p rootfs/workspace

    # setup nix build users for the daemon
    members=""
    for i in $(seq 1 8); do
      echo "nixbld$i:x:$((30000 + i)):30000:nix build user $i:/var/empty:/sbin/nologin" >> rootfs/etc/passwd
      echo "nixbld$i:!::0:::::" >> rootfs/etc/shadow
      members="$members''${members:+,}nixbld$i"
    done
    echo "nixbld:x:30000:$members" >> rootfs/etc/group

    mkdir -p "$out"
    mksquashfs rootfs "$out/store-disk" -comp zstd -Xcompression-level 19 -noappend -no-xattrs -all-root -quiet \
      -p '/sbin/apk m 4755 0 0' # suid apk so spindle-workflow can use it without having to doas or smth
    cp ${kernel} "$out/kernel"
    cp ${initramfs} "$out/initrd"
    cp ${imageSpecJSON} "$out/spec.json"
  ''
