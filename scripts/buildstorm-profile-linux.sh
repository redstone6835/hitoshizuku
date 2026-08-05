#!/bin/sh
# Run the common cold BuildStorm profile flow with a Linux guest kernel.
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

PROFILE_ARCH=${PROFILE_ARCH:-loongarch64}
case "$PROFILE_ARCH" in
    riscv64) linux_dir=$repo/build/linux-riscv64 ;;
    loongarch64) linux_dir=$repo/build/linux ;;
    *) echo "PROFILE_ARCH must be riscv64 or loongarch64" >&2; exit 2 ;;
esac
PROFILE_BOOT_MODE=linux
PROFILE_SYSTEM=${PROFILE_SYSTEM:-linux}
PROFILE_TARGET_FS=${PROFILE_TARGET_FS:-extfs}
PROFILE_KERNEL=${PROFILE_KERNEL:-"$linux_dir/vmlinux"}
PROFILE_LINUX_INITRAMFS=${PROFILE_LINUX_INITRAMFS:-"$repo/build/$PROFILE_ARCH/compat-initramfs.cpio"}
PROFILE_SYMBOL_MAP=${PROFILE_SYMBOL_MAP:-"$linux_dir/System.map"}
PROFILE_SYMBOL_MANIFEST=${PROFILE_SYMBOL_MANIFEST:-"$PROFILE_SYMBOL_MAP.manifest"}
PROFILE_REQUIRE_SYMBOL_MANIFEST=${PROFILE_REQUIRE_SYMBOL_MANIFEST:-1}
PROFILE_QEMU_OBSERVER=${PROFILE_QEMU_OBSERVER:-1}
PROFILE_CAPTURE=${PROFILE_CAPTURE:-0}

export PROFILE_ARCH PROFILE_BOOT_MODE PROFILE_SYSTEM PROFILE_TARGET_FS
export PROFILE_KERNEL PROFILE_LINUX_INITRAMFS
export PROFILE_SYMBOL_MAP PROFILE_SYMBOL_MANIFEST PROFILE_REQUIRE_SYMBOL_MANIFEST
export PROFILE_QEMU_OBSERVER PROFILE_CAPTURE

exec "$repo/scripts/buildstorm-profile-host.sh" "$@"
