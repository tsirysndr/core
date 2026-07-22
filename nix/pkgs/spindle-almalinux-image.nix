{
  doas-sudo-shim-src,
  pkgsStatic,
  runCommand,
  writeText,
  python3,
  squashfsTools,
  spindle-image-helpers,
  binutils,
  kmod,
  libarchive,
  zstd,
  kver,
  rootfs,
  kernel-modules-core,
  kernel-modules,
  uki,
  arch ? "x86_64",
}: let
  # packages pulled from pkgsStatic will have their CA path set to the nix
  # default (/etc/ssl/certs/ca-certificates.crt), which doesn't match the
  # RHEL/AL path, so it'll need to be overridden manually in the various
  # packages
  caPath = "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem";

  busybox = pkgsStatic.busybox;
  nix = pkgsStatic.nixStatic;
  jq = pkgsStatic.jq;
  curlMinimal = pkgsStatic.curlMinimal.overrideAttrs (old: {
    configureFlags = (old.configureFlags or []) ++ ["--with-ca-bundle=${caPath}"];
  });
  git = pkgsStatic.callPackage ./spindle-static-git.nix {
    inherit curlMinimal;
  };
  doas = pkgsStatic.doas.override {
    withPAM = false;
  };
  guestTools = [nix git jq doas];

  pseudoDefinitionsGenerator = writeText "pseudo-definitions-generator.py" ''
    # prints out mksquashfs pseudo-definitions to restore the setuid/caps state
    # from the input tar file

    import base64
    import os.path
    import stat
    import sys
    import tarfile

    with tarfile.open(fileobj=sys.stdin.buffer, mode='r|*') as tar:
        for m in tar:
            name = os.path.normpath(os.path.join('/', m.name))

            if m.mode & (stat.S_ISUID | stat.S_ISGID) != 0:
                print(f'{name} m 0{m.mode & 0o7777:o} {m.uid} {m.gid}')

            for k, v in m.pax_headers.items():
                if k.startswith('LIBARCHIVE.xattr'):
                    raw = base64.b64decode(v)
                elif k.startswith('SCHILY.xattr'):
                    # go from the encoded value back to the raw bytes
                    raw = v.encode('utf-8', 'surrogateescape')
                else:
                    continue

                attr = k.split('.xattr.', 1)[1]
                print(f'{name} x {attr}=0x{raw.hex()}')
  '';

  modulesConf = writeText "spindle-modules.conf" ''
    vmw_vsock_virtio_transport
    # shuttle's cache enqueue listener binds a guest-local (CID 1) vsock
    vsock_loopback
    ext4
  '';

  initService = writeText "spindle-init.service" ''
    [Unit]
    Description=Spindle system initialization

    [Service]
    Type=oneshot
    RemainAfterExit=yes
    # on failure, /workspace is in an unclear state, so don't bother retrying
    Restart=no
    ExecStart=spindle-system-init
  '';

  shuttleService = writeText "shuttle.service" ''
    [Unit]
    Description=Shuttle

    [Service]
    Type=simple
    ExecStart=shuttle
    Environment=NIX_REMOTE=daemon
  '';

  nixSocket = writeText "nix-daemon.socket" ''
    [Unit]
    Description=Nix daemon socket

    [Socket]
    ListenStream=/nix/var/nix/daemon-socket/socket
  '';

  nixService = writeText "nix-daemon.service" ''
    [Unit]
    Description=Nix daemon
    Requires=spindle-init.service
    After=spindle-init.service

    [Service]
    ExecStart=nix-daemon --daemon
    KillMode=process
    Environment=TMPDIR=/workspace/.nix/build
  '';

  doasConf = writeText "doas.conf" ''
    permit nopass spindle-workflow as root
  '';

  imageSpecJSON = writeText "spec.json" (
    builtins.toJSON {
      inherit arch;
      bootArgs = builtins.toString [
        # use the serial console only during early boot, then use the virtio console
        # (if debugging virtio_console issues, the latter to 'console=ttyS0' so
        # logs don't just get eaten)
        "earlyprintk=ttyS0"
        "console=hvc0"

        # reboot via triple fault:
        # https://www.qemu.org/docs/master/system/i386/microvm.html#triggering-a-guest-initiated-shut-down
        # > The recommended way to trigger a guest-initiated shut down is by
        # > generating a triple-fault, > which will cause the VM to initiate a
        # > reboot".
        "reboot=triple"
        # reboot immediately on panic
        "panic=-1"

        "root=/dev/vda"
        "rootfstype=squashfs"
        # volatile rootfs
        "systemd.volatile=overlay"
        # selinux is not particularly useful for microvm CI
        "selinux=0"

        # enable this to forward journal logs to the console, for debugging
        # "systemd.journald.forward_to_console=1"
      ];
      kernel = "kernel";
      initrd = "initrd";
      runnerType = "qemu";
      runnerConfig = {
        cpu = "host,+x2apic,-sgx";
        machine = "microvm,accel=kvm:tcg,acpi=on,mem-merge=on,pcie=on,pic=off,pit=off,rtc=on,usb=off";
        console = "hvc0";
        extraArgs = [];
        # RHEL/AlmaLinux kernels are built with CONFIG_VIRTIO_MMIO, so use PCI
        # instead (this is why pcie=on in the `machine` line above, instead of
        # `pcie=off` like other VM images)
        virtioTransport = "pci";
      };
      memoryMiB = 4096;
      storeDisk = "store-disk";
      storeDiskType = "squashfs";
      vcpus = 2;
      shell = "/usr/bin/bash";
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
  runCommand "spindle-almalinux-image-${arch}" {
    nativeBuildInputs = [squashfsTools binutils kmod libarchive python3 zstd];
  } ''
    mkdir -p rootfs
    bsdtar -xpf ${rootfs} -C rootfs --no-xattrs
    # we'll need to write to here later
    chmod -R u+w rootfs/{usr/{bin,sbin},etc/shadow}

    # set up the pseudo definitions to restore setuid/caps at the end
    python3 ${pseudoDefinitionsGenerator} < ${rootfs} > pseudo-file.txt
    # doas will need setuid, too
    echo '/usr/local/bin/doas m 04755 0 0' >> pseudo-file.txt

    # extract the uki to get the kernel + initramfs
    # (the UKI is used here because there is no better-suitable initramfs
    # available standalone—the network booting version is utterly massive and
    # contains many modules not needed for the microvm)
    bsdtar -xf ${uki} --strip-components=4 './lib/modules/*/vmlinuz-virt.efi'
    objcopy -O binary -j.linux vmlinuz-virt.efi vmlinuz
    objcopy -O binary -j.initrd vmlinuz-virt.efi initrd

    # add some additional necessary modules to the initrd
    mkdir initrd-tree
    bsdtar -C initrd-tree -xf initrd
    # modules and dependencies:
    # - squashfs
    # - virtio_console
    # - virtio_net -> net_failover -> failover
    # - vmw_vsock_virtio_transport -> [a bunch of other stuff in vmw_vsock]
    bsdtar -C initrd-tree/usr -xf ${kernel-modules-core} \
      "./lib/modules/${kver}/kernel/drivers/char/virtio_console.ko.xz" \
      "./lib/modules/${kver}/kernel/drivers/net/net_failover.ko.xz" \
      "./lib/modules/${kver}/kernel/drivers/net/virtio_net.ko.xz" \
      "./lib/modules/${kver}/kernel/net/core/failover.ko.xz" \
      "./lib/modules/${kver}/kernel/net/vmw_vsock/*"
    bsdtar -C initrd-tree/usr -xf ${kernel-modules} \
      "./lib/modules/${kver}/kernel/fs/squashfs/squashfs.ko.xz"
    find initrd-tree -name '*.ko.xz' -exec xz -d '{}' +
    depmod -b initrd-tree ${kver}  # regenerate modules.dep
    # place the modules-load.d configuration in the initramfs, because
    # systemd-modules-load does not exist in the default rootfs
    install -D -m 0644 ${modulesConf} initrd-tree/etc/modules-load.d/spindle-modules.conf
    (cd initrd-tree && find . | bsdtar --format=newc --uid=0 --gid=0 -cnf - -T -) \
      | zstd -12 > initrd

    cp -a initrd-tree/usr/lib/modules rootfs/usr/lib

    ${spindle-image-helpers.setupRootfs} rootfs

    install -D -m 0644 ${initService} rootfs/usr/lib/systemd/system/spindle-init.service
    install -D -m 0644 ${shuttleService} rootfs/usr/lib/systemd/system/shuttle.service

    # nix daemon
    install -D -m 0644 ${nixSocket} rootfs/usr/lib/systemd/system/nix-daemon.socket
    install -D -m 0644 ${nixService} rootfs/usr/lib/systemd/system/nix-daemon.service
    echo 'ssl-cert-file = ${caPath}' >> rootfs/etc/nix/nix.conf  # add the SSL configuration

    # doas
    install -D -m 0644 ${doasConf} rootfs/etc/doas.conf
    install -Dm 755 ${doas-sudo-shim-src}/sudo -t rootfs/usr/local/bin

    # enable some services by default
    ln -s \
      ../nix-daemon.socket \
      ../spindle-init.service \
      ../shuttle.service \
      rootfs/usr/lib/systemd/system/multi-user.target.wants/

    # install dependencies
    ${spindle-image-helpers.installGuestTools} rootfs ${toString guestTools}

    # busybox is needed for the ip tools to work (iproute2 is a MUCH larger
    # dependency), but we don't include it in the above loop because we don't
    # want any of the symlinks (since AL already ships coreutils)
    cp ${busybox}/bin/busybox rootfs/usr/local/bin

    mkdir -p "$out"
    mksquashfs rootfs "$out/store-disk" \
      -comp zstd -Xcompression-level 19 \
      -noappend -all-root -quiet \
      -pf pseudo-file.txt
    cp vmlinuz "$out/kernel"
    cp initrd "$out/initrd"
    cp ${imageSpecJSON} "$out/spec.json"
  ''
