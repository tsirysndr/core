{
  config,
  pkgs,
  lib,
  ...
}: let
  # these are modules / module trees we keep. everything else is pruned.
  # this is a "cheap" way to save on what we ship, we don't have to recompile anything.
  # this saves about 118mb!
  keepTrees = [
    "crypto"
    "lib"
    "arch"
    "drivers/virtio"
    # net: the firewall modprobes across netfilter/ipv4/ipv6; docker adds the
    # bridge/llc/802(stp)/xfrm machinery + NAT targets in netfilter.
    "net/core"
    "net/netfilter"
    "net/ipv4"
    "net/ipv6"
    "net/packet"
    "net/sched"
    "net/vmw_vsock"
    "net/bridge"
    "net/llc"
    "net/802"
    "net/xfrm"
    "fs/configfs"
    "fs/autofs"
    "fs/nls"
    "fs/unicode"
  ];
  keepMods = [
    # boot + storage + common workflow filesystems
    "erofs"
    "ext4"
    "jbd2"
    "mbcache"
    "overlay"
    "fuse"
    "loop"
    # "btrfs"
    # "xfs"
    # "f2fs"
    # "vfat"
    # "exfat"
    "squashfs"
    "isofs"
    "dm-mod"
    "zram"
    # virtio devices the runner exposes
    "virtio"
    "virtio_mmio"
    "virtio_pci"
    "virtio_blk"
    "virtio_net"
    "virtio_rng"
    "virtio_console"
    "vsock_loopback"
    "vmw_vsock_virtio_transport"
    "vmw_vsock_virtio_transport_common"
    # container networking (docker default bridge + common custom networks)
    "veth"
    "tun"
    "tap"
    "bridge"
    "br_netfilter"
    "macvlan"
    "ipvlan"
    "vxlan"
    "geneve"
    "dummy"
    "wireguard"
  ];
  keepTreesFile = pkgs.writeText "keep-trees" (lib.concatStringsSep "\n" keepTrees);
  keepModsFile = pkgs.writeText "keep-mods" (lib.concatStringsSep "\n" keepMods);
  slimModulesScript = pkgs.writeText "slim-modules.py" ''
    import os, shutil, sys

    src, dst, trees_file, mods_file = sys.argv[1:5]
    KEEP_TREES = open(trees_file).read().split()
    KEEP_MODS = open(mods_file).read().split()

    def norm(name):
        return name.replace("-", "_")

    kerneldir = os.path.join(src, "kernel")

    bypath, byname = {}, {}
    for root, _, files in os.walk(kerneldir):
        for f in files:
            if ".ko" not in f:
                continue
            ap = os.path.join(root, f)
            rel = os.path.relpath(ap, src)
            bypath[rel] = ap
            byname[norm(f.split(".ko")[0])] = rel

    deps = {}
    with open(os.path.join(src, "modules.dep")) as fh:
        for line in fh:
            if ":" in line:
                mod, rest = line.split(":", 1)
                deps[mod.strip()] = rest.split()

    keep = set()
    def add(rel):
        if rel in keep or rel not in bypath:
            return
        keep.add(rel)
        for dep in deps.get(rel, []):
            add(dep)

    for tree in KEEP_TREES:
        for root, _, files in os.walk(os.path.join(kerneldir, tree)):
            for f in files:
                if ".ko" in f:
                    add(os.path.relpath(os.path.join(root, f), src))
    for mod in KEEP_MODS:
        rel = byname.get(norm(mod))
        if rel:
            add(rel)

    for rel in keep:
        target = os.path.join(dst, rel)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        shutil.copy2(bypath[rel], target)
    print(f"kept {len(keep)} of {len(bypath)} modules")
  '';
  slimKernelModules =
    pkgs.runCommand "${config.boot.kernelPackages.kernel.name}-modules-microvm"
    {nativeBuildInputs = [pkgs.python3 pkgs.kmod];}
    ''
      src=${lib.getOutput "modules" config.boot.kernelPackages.kernel}/lib/modules
      ver=$(ls "$src")
      mkdir -p "$out/lib/modules/$ver"
      for f in "$src/$ver"/modules.builtin* "$src/$ver"/modules.order; do
        [ -e "$f" ] && cp "$f" "$out/lib/modules/$ver/"
      done
      python3 ${slimModulesScript} "$src/$ver" "$out/lib/modules/$ver" ${keepTreesFile} ${keepModsFile}
      # regen modules.dep
      depmod -b "$out" "$ver"
    '';
in {
  system.stateVersion = "26.05";

  # actually use our slimmed down modules set
  system.modulesTree = lib.mkForce ([slimKernelModules] ++ config.boot.extraModulePackages);

  boot.initrd.includeDefaultModules = lib.mkForce false;
  boot.initrd.availableKernelModules = lib.mkForce [];
  boot.initrd.kernelModules = lib.mkForce [
    "virtio_pci"
    "virtio_mmio"
    "virtio_blk"
    "virtio_console"
    "erofs"
    "ext4"
    "overlay"
  ];
  boot.kernelModules = ["loop"];

  # some zram to help situations where burst memory usage causes OOM
  zramSwap = {
    enable = true;
    algorithm = "zstd";
    memoryPercent = 50;
  };

  programs.nano.enable = false;
  # we are on a microvm we don't need the hardware map
  environment.etc."udev/hwdb.bin".enable = lib.mkForce false;

  networking.hostName = "spindle-microvm";
  networking.useDHCP = false;
  systemd.network.networks."40-eth0" = {
    matchConfig.Name = "eth0";
    address = ["10.0.3.15/24"];
    gateway = ["10.0.3.2"];
    dns = ["127.0.0.1"];
  };
  networking.nameservers = ["127.0.0.1"];

  # this is disabled by microvm optimizations but we do need it
  system.switch.enable = lib.mkForce true;

  # don't install docs or any xdg things, not necessary
  documentation.enable = false;
  xdg.mime.enable = false;
  xdg.icons.enable = false;
  xdg.sounds.enable = false;

  users.groups.spindle-workflow = {
    gid = 970;
  };
  users.users.spindle-workflow = {
    isSystemUser = true;
    uid = 970;
    group = "spindle-workflow";
    home = "/workspace";
    createHome = false;
  };
  users.users.spindle-workflow.extraGroups = lib.mkIf config.virtualisation.docker.enable [
    "docker"
  ];
  virtualisation.docker.listenOptions = [
    "/run/docker.sock"
    "/var/run/docker.sock"
  ];

  nix = {
    settings = {
      experimental-features = [
        "nix-command"
        "flakes"
      ];
      trusted-users = ["root"];
      allowed-users = ["spindle-workflow"];
    };
    registry.nixpkgs.to = {
      type = "path";
      path = pkgs.path;
    };
    extraOptions = ''
      extra-experimental-features = nix-command flakes
      !include /run/spindle/nix.conf
    '';
    nixPath = ["nixpkgs=${config.nix.registry.nixpkgs.to.path}"];
  };

  systemd.tmpfiles.rules = [
    "d /run/spindle 0755 root root -"
    "d /workspace 0755 spindle-workflow spindle-workflow -"
    "d /workspace/repo 0755 spindle-workflow spindle-workflow -"
  ];

  # nix flake cache should go on the persisted dir, not the root (which is on tmpfs)
  systemd.services.shuttle = {
    environment.XDG_CACHE_HOME = "/var/cache/shuttle";
    serviceConfig.CacheDirectory = "shuttle";
  };

  # add any common packages / services here
  environment.systemPackages = with pkgs; [
    gitMinimal
    curlMinimal
    wget
    coreutils-full
    file
    findutils
    gnused
    jq
    yq
    xxd
    gnutar
    zip
    unzip
    gz-utils
    bzip2
    lz4
    p7zip
  ];
  # disable default nixos packages ([perl rsync strace])
  environment.defaultPackages = [];
  # this removed nixos-rebuild-ng and nixos-generate-config, which lets us
  # remove python3 closure (~107MB)
  system.disableInstallerTools = true;

  # a single volume that will back /workspace, /var, and the nix store
  microvm.storeOnDisk = true;
  microvm.storeDiskType = "erofs";
  # lz4hc, not zstd: the stock nixpkgs kernel builds erofs without
  # CONFIG_EROFS_FS_ZIP_ZSTD, so a zstd image fails to mount at boot ("algorithm
  # 3 isn't enabled on this kernel"); only lz4 is guaranteed. -Efragments and
  # -Ededupe are omitted because microvm.nix falls back to single-threaded
  # erofs-utils when either is present, which makes image builds really slow.
  # for now, we take the compression hit, which isn't too much anyway.
  # todo(dawn): the remaining big save needs a custom guest kernel (we'd want a
  # binary cache first so downstream users don't rebuild it every time): enable
  # EROFS_FS_ZIP_ZSTD for a better-compressing store-disk, build the essentials
  # (virtio/erofs/ext4/overlay/netfilter) in as =y, and strip the kernel image
  # itself. the modules tree is already pruned without a recompile, see
  # slimKernelModules above.
  microvm.storeDiskErofsFlags = [
    "-zlz4hc"
    "-Eztailpacking"
    "-C131072" # bigger compression window lets lz4hc compress better (~47mb)
  ];
  microvm.writableStoreOverlay = "/persist/rw-store";
  microvm.volumes = [
    {
      image = "persist.img";
      mountPoint = "/persist";
      size = 1024 * 24; # 24 GB
      fsType = "ext4";
    }
  ];

  # /persist must be mounted before the writable store overlay activates
  fileSystems."/persist".neededForBoot = true;

  fileSystems."/workspace" = {
    device = "/persist/workspace";
    fsType = "none";
    options = ["bind"];
    depends = ["/persist"];
  };
  # bind mounting /var is important since docker etc. can't use overlayfs
  # (overlayfs on overlayfs does not work)
  fileSystems."/var" = {
    device = "/persist/var";
    fsType = "none";
    options = ["bind"];
    depends = ["/persist"];
  };
  # bind mount /tmp so our tmp is disk backed...
  fileSystems."/tmp" = {
    device = "/persist/tmp";
    fsType = "none";
    options = ["bind"];
    depends = ["/persist"];
  };

  # create bind sources before local-fs.target, which means we have to do this
  # at initrd time
  boot.initrd.systemd.enable = true;
  boot.initrd.systemd.tmpfiles.settings."00-persist-layout" = {
    "/sysroot/persist/rw-store".d = {
      mode = "0755";
    };
    "/sysroot/persist/workspace".d = {
      mode = "0755";
      user = "spindle-workflow";
      group = "spindle-workflow";
    };
    "/sysroot/persist/var".d = {
      mode = "0755";
    };
    "/sysroot/persist/tmp".d = {
      mode = "1777";
    };
  };
}
