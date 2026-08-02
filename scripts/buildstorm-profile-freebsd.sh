#!/bin/sh
# Run BuildStorm profile with a FreeBSD guest kernel (LoongArch64).
#
# FreeBSD LoongArch64 status: experimental (FreeBSD 15-CURRENT)
# Build from source: make TARGET=loongarch TARGET_ARCH=loongarch64 buildworld buildkernel
# Reference: https://wiki.freebsd.org/LoongArch
#
# Required env vars:
#   PROFILE_KERNEL             FreeBSD LoongArch64 kernel ELF
#   PROFILE_SYMBOL_MAP         FreeBSD kernel symbol map (nm -n format)
set -eu

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

PROFILE_BOOT_MODE=freebsd
PROFILE_SYSTEM=${PROFILE_SYSTEM:-freebsd}
PROFILE_SYMBOL_MAP=${PROFILE_SYMBOL_MAP:-""}
PROFILE_SYMBOL_MANIFEST=${PROFILE_SYMBOL_MANIFEST:-""}
PROFILE_REQUIRE_SYMBOL_MANIFEST=${PROFILE_REQUIRE_SYMBOL_MANIFEST:-0}
PROFILE_QEMU_OBSERVER=${PROFILE_QEMU_OBSERVER:-1}
PROFILE_CAPTURE=${PROFILE_CAPTURE:-0}

export PROFILE_BOOT_MODE PROFILE_SYSTEM
export PROFILE_SYMBOL_MAP PROFILE_SYMBOL_MANIFEST PROFILE_REQUIRE_SYMBOL_MANIFEST
export PROFILE_QEMU_OBSERVER PROFILE_CAPTURE

exec "$repo/scripts/buildstorm-profile-host.sh" "$@"
