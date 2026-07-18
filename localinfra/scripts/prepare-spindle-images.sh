#!/usr/bin/env bash
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
image_root="${1:-$repo/out/localinfra-spindle-images}"

mkdir -p "$image_root"

extract_image() {
    local package="$1"
    local name="$2"
    shift 2

    local tarball
    tarball=$(nix build "$repo#$package" --no-link --print-out-paths)

    [ -d "$image_root/$name" ] && chmod -R +w "$image_root/$name" || true
    rm -rf "$image_root/$name"
    mkdir -p "$image_root/$name"
    tar -C "$image_root/$name" -xzf "$tarball"

    local alias
    for alias in "$@"; do
        rm -rf "$image_root/$alias"
        ln -s "$name" "$image_root/$alias"
    done
}

extract_image spindle-nixos-image-tarball nixos-x86_64 nixos
extract_image spindle-alpine-image-tarball alpine-x86_64 alpine
extract_image spindle-almalinux10-image-tarball almalinux10-x86_64 almalinux10 almalinux

# a reduced image set (alpine only) for the mill overlay: an executor mounting
# this advertises image/alpine but not image/nixos, so capability-based
# placement is testable across a mixed fleet
alpine_only="$repo/out/localinfra-spindle-images-alpine"
[ -d "$alpine_only" ] && chmod -R +w "$alpine_only" || true
rm -rf "$alpine_only"
mkdir -p "$alpine_only"
cp -r "$image_root/alpine-x86_64" "$alpine_only/alpine-x86_64"
ln -s alpine-x86_64 "$alpine_only/alpine"

echo "prepared spindle microVM images in $image_root"
