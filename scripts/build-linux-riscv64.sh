#!/bin/sh
# 从只读 distfiles 还原并构建与宿主 Gentoo 内核版本一致的 RISC-V64 Linux。
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
image=${LINUX_BUILD_CONTAINER:-zhouzhouyi/os-contest:20260510}
base_archive=${LINUX_BASE_ARCHIVE:-/var/cache/distfiles/linux-6.18.tar.xz}
stable_patch=${LINUX_STABLE_PATCH:-/var/cache/distfiles/patch-6.18.37.xz}
gentoo_patches=${LINUX_GENTOO_PATCHES:-/var/cache/distfiles/linux-gentoo-patches-6.18.37_p1.tar.xz}
jobs=${JOBS:-$(nproc 2>/dev/null || echo 4)}
timestamp=${KBUILD_BUILD_TIMESTAMP:-2026-08-05 00:00:00 UTC}

for input in "$base_archive" "$stable_patch" "$gentoo_patches"; do
    [ -r "$input" ] || {
        echo "Linux RISC-V build input is unreadable: $input" >&2
        exit 1
    }
done
case "$jobs" in ''|*[!0-9]*|0) echo "JOBS must be a positive integer" >&2; exit 2 ;; esac

docker run --rm \
    -v "$root":/work \
    -v "$base_archive":/src/linux-base.tar.xz:ro \
    -v "$stable_patch":/src/linux-stable.patch.xz:ro \
    -v "$gentoo_patches":/src/linux-gentoo-patches.tar.xz:ro \
    -w /work "$image" bash -euxo pipefail -c '
        apt-get update
        DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            bc bison flex libelf-dev libssl-dev

        source_dir=/work/build/linux-riscv64-src
        output_dir=/work/build/linux-riscv64
        rm -rf "$source_dir" "$output_dir"
        mkdir -p "$source_dir" "$output_dir" /tmp/gentoo-patches
        tar -xf /src/linux-base.tar.xz -C "$source_dir" --strip-components=1
        xz -dc /src/linux-stable.patch.xz | \
            patch -d "$source_dir" -p1 --batch --forward
        tar -xf /src/linux-gentoo-patches.tar.xz -C /tmp/gentoo-patches \
            --strip-components=1
        for patch_file in /tmp/gentoo-patches/*.patch; do
            patch -d "$source_dir" -p1 --batch --forward <"$patch_file"
        done

        cp "$source_dir/arch/riscv/configs/defconfig" "$output_dir/.config"
        "$source_dir/scripts/config" --file "$output_dir/.config" \
            -d DEBUG_INFO_NONE -e DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT \
            -d DEBUG_INFO_BTF -d RANDOMIZE_BASE -d RELOCATABLE \
            -e KALLSYMS -e KALLSYMS_ALL -e BLK_DEV_INITRD \
            -e DEVTMPFS -e DEVTMPFS_MOUNT -e SERIAL_8250_CONSOLE \
            -e VIRTIO_MMIO \
            --set-str SYSTEM_TRUSTED_KEYS "" \
            --set-str SYSTEM_REVOCATION_KEYS ""
        make -C "$source_dir" O="$output_dir" ARCH=riscv \
            CROSS_COMPILE=riscv64-linux-gnu- olddefconfig
        make -C "$source_dir" O="$output_dir" ARCH=riscv \
            CROSS_COMPILE=riscv64-linux-gnu- \
            KBUILD_BUILD_USER=mygo KBUILD_BUILD_HOST=os-contest \
            KBUILD_BUILD_TIMESTAMP="$1" -j"$2" vmlinux Image

        kernel_sha=$(sha256sum "$output_dir/vmlinux" | cut -d" " -f1)
        map_sha=$(sha256sum "$output_dir/System.map" | cut -d" " -f1)
        config_sha=$(sha256sum "$output_dir/.config" | cut -d" " -f1)
        base_sha=$(sha256sum /src/linux-base.tar.xz | cut -d" " -f1)
        stable_sha=$(sha256sum /src/linux-stable.patch.xz | cut -d" " -f1)
        gentoo_sha=$(sha256sum /src/linux-gentoo-patches.tar.xz | cut -d" " -f1)
        printf "%s\n" \
            "schema=mygo.kernel-map-manifest.v1" \
            "target=riscv64-linux-gnu" \
            "linux_release=6.18.37-p1-gentoo" \
            "kernel_sha256=$kernel_sha" \
            "symbol_map_sha256=$map_sha" \
            "config_sha256=$config_sha" \
            "base_archive_sha256=$base_sha" \
            "stable_patch_sha256=$stable_sha" \
            "gentoo_patches_sha256=$gentoo_sha" \
            >"$output_dir/System.map.manifest"
    ' build-linux-riscv64 "$timestamp" "$jobs"

printf 'Linux RISC-V kernel: %s\n' "$root/build/linux-riscv64/vmlinux"
printf 'Linux RISC-V Image:  %s\n' "$root/build/linux-riscv64/arch/riscv/boot/Image"
