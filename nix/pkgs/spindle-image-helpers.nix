{
  shuttle,
  writeScript,
  writeShellScript,
  writeText,
  publicsuffix-list,
}: let
  nixConf = writeText "nix.conf" ''
    # mirror nix/microvm/base.nix and nix/modules/shuttle.nix
    experimental-features = nix-command flakes
    trusted-users = root spindle-workflow
    allowed-users = spindle-workflow
    post-build-hook = /usr/libexec/spindle-post-build-hook
    # keep build sandboxes on the /workspace disk, not the RAM-backed root tmpfs
    build-dir = /workspace/.nix/build
    !include /run/spindle/nix.conf
  '';

  postBuildHook = writeText "spindle-post-build-hook" ''
    #!/bin/sh
    set -f

    if [ -z "''${OUT_PATHS:-}" ]; then
      exit 0
    fi

    # OUT_PATHS is intentionally split into individual store paths
    exec shuttle enqueue-built-paths $OUT_PATHS
  '';

  systemInit = writeScript "spindle-system-init.sh" ''
    #!/bin/sh
    # setup xdg runtime dir, podman eg. needs it
    install -d -m 0700 -o spindle-workflow -g spindle-workflow /run/user/970

    if [ -b /dev/vdb ]; then
      # setup disk backed nix store
      mount -t ext4 /dev/vdb /workspace
      install -d -o spindle-workflow -g spindle-workflow /workspace /workspace/repo
      install -d /workspace/.nix/rw-store /workspace/.nix/rw-store-work /workspace/.nix/build
      mount -t overlay overlay \
        -o lowerdir=/nix/store,upperdir=/workspace/.nix/rw-store,workdir=/workspace/.nix/rw-store-work \
        /nix/store
    fi

    busybox ip link set lo up
    busybox ip link set eth0 up
    busybox ip addr add 10.0.3.15/24 dev eth0
    busybox ip route add default via 10.0.3.2
  '';
in {
  setupRootfs = writeShellScript "setup-rootfs" ''
    set -eu

    rootfs="$1"
    shift

    install -D -m 0755 ${shuttle}/bin/shuttle "$rootfs"/usr/bin/shuttle
    install -D -m 0644 ${nixConf} "$rootfs"/etc/nix/nix.conf
    install -D -m 0755 ${postBuildHook} "$rootfs"/usr/libexec/spindle-post-build-hook
    install -D -m 0755 ${systemInit} "$rootfs"/sbin/spindle-system-init

    # this is necessary for nix to work, it is not a library but nix hardcodes
    # it in it's binary
    mkdir -p "$rootfs"/nix/store
    cp -rv ${publicsuffix-list} "$rootfs"/nix/store/

    echo "spindle-microvm" > "$rootfs"/etc/hostname
    printf 'nameserver 127.0.0.1\n' > "$rootfs"/etc/resolv.conf

    echo "spindle-workflow:x:970:970:spindle workflow:/workspace:/bin/sh" >> "$rootfs"/etc/passwd
    echo "spindle-workflow:x:970:" >> "$rootfs"/etc/group
    echo "spindle-workflow:!::0:::::" >> "$rootfs"/etc/shadow
    mkdir -p "$rootfs"/workspace

    # subordinate id ranges so the workflow user can run rootless containers
    # (podman/buildah): without these, user-namespace id mapping falls back to a
    # single 970->0 map and any layer that chowns to another uid fails. the range
    # is well clear of 970 and the 30000-block nixbld users.
    echo "spindle-workflow:100000:65536" >> "$rootfs"/etc/subuid
    echo "spindle-workflow:100000:65536" >> "$rootfs"/etc/subgid

    # setup nix build users for the daemon
    members=""
    for i in $(seq 1 8); do
      echo "nixbld$i:x:$((30000 + i)):30000:nix build user $i:/var/empty:/sbin/nologin" >> "$rootfs"/etc/passwd
      echo "nixbld$i:!::0:::::" >> "$rootfs"/etc/shadow
      members="$members''${members:+,}nixbld$i"
    done
    echo "nixbld:x:30000:$members" >> "$rootfs"/etc/group
  '';

  installGuestTools = writeShellScript "install-guest-tools" ''
    set -eu

    rootfs="$1"
    shift

    # we only copy binaries + libexec for minimal deps so the image size doesn't
    # increase so much (if we copy the whole guestTools closure for example, it
    # doubles the disk size)
    mkdir -p "$rootfs"/nix/store "$rootfs"/usr/local/bin
    for pkg in "$@"; do
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
          ln -vsf "$real" "$rootfs/usr/local/bin/$name"
        else
          cp -v "$real" "$rootfs/usr/local/bin/$name"
        fi
      done
      # libexec has binaries used by packages even if statically compiled
      if [[ -d "$pkg/libexec" ]]; then
        mkdir -p "$rootfs$pkg"
        cp -av "$pkg/libexec" "$rootfs$pkg/"
      fi
    done
  '';
}
