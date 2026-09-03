#!/bin/sh
# Build a complete Alpine x86_64 userspace and a bootable disk/ISO pair.
#
# The kernel image is a Multiboot2 higher-half ELF, so the ISO contains GRUB
# and the kernel while the Alpine root lives on a GPT VirtIO disk. Keeping the
# two artifacts separate makes it possible to update the kernel without
# rebuilding the distribution image.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root=$repo_root/build
build_tmp=${HITOSHIZUKU_BUILD_TMPDIR:-$build_root/alpine-tmp}

alpine_version=${ALPINE_VERSION:-3.24.1}
alpine_branch=${ALPINE_BRANCH:-3.24}
alpine_arch=x86_64
alpine_mirror=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
alpine_sha256=${ALPINE_SHA256:-}

rootfs_dir=${ALPINE_ROOTFS_DIR:-$build_root/alpine/alpine-x86_64}
image_path=${ALPINE_IMAGE:-$build_root/alpine/alpine-x86_64.img}
iso_path=${ALPINE_ISO:-$build_root/x86_64/alpine-boot.iso}
cache_dir=${ALPINE_CACHE_DIR:-$build_root/cache/alpine}
image_size=${ALPINE_IMAGE_SIZE:-2G}
root_device=${ALPINE_ROOT_DEVICE:-/dev/vd0p1}
console_device=${ALPINE_CONSOLE:-uart0}
partitioned=${ALPINE_PARTITIONED:-1}
nameservers=${ALPINE_NAMESERVERS:-}
if [ -z "$nameservers" ] && [ -r /etc/resolv.conf ]; then
    nameservers=$(awk \
        '$1 == "nameserver" && $2 !~ /^127\./ && $2 != "::1" { print $2 }' \
        /etc/resolv.conf)
fi
if [ -z "$nameservers" ]; then
    nameservers='1.1.1.1 9.9.9.9'
fi

# This is intentionally a broad development/server image. It does not install
# another kernel: Hitoshizuku supplies the kernel and the image is booted by
# its own Multiboot2 path.
default_packages='alpine-base alpine-conf openrc bash zsh coreutils util-linux e2fsprogs dosfstools pciutils usbutils iproute2 iputils dhcpcd ethtool nftables openssh curl wget ca-certificates tzdata procps psmisc findutils grep sed gawk less nano vim tar gzip bzip2 xz zstd lsof strace tcpdump socat file which shadow doas sudo musl-locales man-pages mandoc git make build-base pkgconf python3 py3-pip perl tmux screen rsync'
packages=${ALPINE_PACKAGES:-$default_packages}

need_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "alpine image: missing required command: $1" >&2
        exit 1
    }
}

for command in \
    awk chroot curl dd e2fsck find grub2-file grub2-mkrescue \
    mkfs.ext4 mkdir mv numfmt readelf realpath sfdisk sha256sum sort tar truncate unshare
do
    need_command "$command"
done

rootfs_dir=$(realpath -m -- "$rootfs_dir")
image_path=$(realpath -m -- "$image_path")
iso_path=$(realpath -m -- "$iso_path")
cache_dir=$(realpath -m -- "$cache_dir")
build_root=$(realpath -m -- "$build_root")
build_tmp=$(realpath -m -- "$build_tmp")

validate_output() {
    output=$1
    label=$2
    case "$output" in
        "$build_root"/*) ;;
        *)
            echo "alpine image: $label must stay below $build_root: $output" >&2
            exit 1
            ;;
    esac
}

validate_output "$rootfs_dir" ALPINE_ROOTFS_DIR
validate_output "$image_path" ALPINE_IMAGE
validate_output "$iso_path" ALPINE_ISO
validate_output "$cache_dir" ALPINE_CACHE_DIR

validate_disjoint() {
    left=$1
    left_label=$2
    right=$3
    right_label=$4
    case "$left/" in
        "$right/"|"$right/"*)
            echo "alpine image: $left_label and $right_label must not overlap" >&2
            echo "  $left_label=$left" >&2
            echo "  $right_label=$right" >&2
            exit 1
            ;;
    esac
    case "$right/" in
        "$left/"*)
            echo "alpine image: $left_label and $right_label must not overlap" >&2
            echo "  $left_label=$left" >&2
            echo "  $right_label=$right" >&2
            exit 1
            ;;
    esac
}

validate_disjoint "$rootfs_dir" ALPINE_ROOTFS_DIR "$image_path" ALPINE_IMAGE
validate_disjoint "$rootfs_dir" ALPINE_ROOTFS_DIR "$iso_path" ALPINE_ISO
validate_disjoint "$rootfs_dir" ALPINE_ROOTFS_DIR "$cache_dir" ALPINE_CACHE_DIR
validate_disjoint "$rootfs_dir" ALPINE_ROOTFS_DIR "$build_tmp" HITOSHIZUKU_BUILD_TMPDIR
validate_disjoint "$image_path" ALPINE_IMAGE "$iso_path" ALPINE_ISO
validate_disjoint "$image_path" ALPINE_IMAGE "$cache_dir" ALPINE_CACHE_DIR
validate_disjoint "$image_path" ALPINE_IMAGE "$build_tmp" HITOSHIZUKU_BUILD_TMPDIR
validate_disjoint "$iso_path" ALPINE_ISO "$cache_dir" ALPINE_CACHE_DIR
validate_disjoint "$iso_path" ALPINE_ISO "$build_tmp" HITOSHIZUKU_BUILD_TMPDIR
validate_disjoint "$cache_dir" ALPINE_CACHE_DIR "$build_tmp" HITOSHIZUKU_BUILD_TMPDIR

mkdir -p "$build_tmp"
export TMPDIR=$build_tmp

case "$alpine_version$alpine_branch" in
    *[!A-Za-z0-9._-]*)
        echo "alpine image: release version/branch contains unsafe characters" >&2
        exit 1
        ;;
esac

case "$partitioned" in
    0|1) ;;
    *) echo "alpine image: ALPINE_PARTITIONED must be 0 or 1" >&2; exit 1 ;;
esac
case "$root_device" in
    /dev/vd0|/dev/vd0p1) ;;
    *) echo "alpine image: ALPINE_ROOT_DEVICE must be /dev/vd0 or /dev/vd0p1" >&2; exit 1 ;;
esac
case "$console_device" in
    uart0) ;;
    *) echo "alpine image: ALPINE_CONSOLE must be uart0" >&2; exit 1 ;;
esac
if [ "$partitioned" = 1 ] && [ "$root_device" != /dev/vd0p1 ]; then
    echo "alpine image: partitioned images require ALPINE_ROOT_DEVICE=/dev/vd0p1" >&2
    exit 1
fi

for nameserver in $nameservers; do
    case "$nameserver" in
        ''|*[!0-9A-Fa-f.:]*)
            echo "alpine image: invalid nameserver address: $nameserver" >&2
            exit 1
            ;;
    esac
done
if [ "$partitioned" = 0 ] && [ "$root_device" != /dev/vd0 ]; then
    echo "alpine image: raw images require ALPINE_ROOT_DEVICE=/dev/vd0" >&2
    exit 1
fi

image_bytes=$(numfmt --from=iec "$image_size" 2>/dev/null || true)
case "$image_bytes" in
    ''|*[!0-9]*)
        echo "alpine image: invalid ALPINE_IMAGE_SIZE=$image_size" >&2
        exit 1
        ;;
esac
image_bytes=$((image_bytes / 512 * 512))
if [ "$image_bytes" -lt $((256 * 1024 * 1024)) ]; then
    echo "alpine image: image must be at least 256 MiB" >&2
    exit 1
fi

archive_name="alpine-minirootfs-${alpine_version}-${alpine_arch}.tar.gz"
archive_url="${alpine_mirror}/v${alpine_branch}/releases/${alpine_arch}/${archive_name}"
archive_path=$cache_dir/$archive_name
checksum_path=$cache_dir/$archive_name.sha256

mkdir -p "$cache_dir" "$(dirname -- "$rootfs_dir")" \
    "$(dirname -- "$image_path")" "$(dirname -- "$iso_path")"

if [ ! -s "$archive_path" ]; then
    echo "alpine image: downloading $archive_url"
    curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
        --output "$archive_path.tmp" "$archive_url"
    mv "$archive_path.tmp" "$archive_path"
fi

# The default release is pinned. Any other release must be supplied with an
# explicit digest so a moving mirror cannot silently change the image.
if [ -z "$alpine_sha256" ] && [ "$alpine_version" = 3.24.1 ]; then
    alpine_sha256=41f73e3cf5fa919b8aa5ca6b30dc48f0da2720776d7423e2a7748211456fe081
fi
if [ -z "$alpine_sha256" ]; then
    echo "alpine image: set ALPINE_SHA256 when using Alpine $alpine_version" >&2
    exit 1
fi
actual_sha256=$(sha256sum "$archive_path" | awk '{print $1}')
if [ "$actual_sha256" != "$alpine_sha256" ]; then
    echo "alpine image: SHA-256 mismatch for $archive_name" >&2
    echo "  expected: $alpine_sha256" >&2
    echo "  actual:   $actual_sha256" >&2
    exit 1
fi
printf '%s  %s\n' "$alpine_sha256" "$archive_name" >"$checksum_path"

kernel_path=${HITOSHIZUKU_KERNEL:-$build_root/x86_64/kernel.elf}
if [ ! -f "$kernel_path" ]; then
    echo "alpine image: missing $kernel_path" >&2
    echo "  build it first: cargo xtask image --platform qemu-x86_64 --format elf" >&2
    exit 1
fi
if ! grub2-file --is-x86-multiboot2 "$kernel_path"; then
    echo "alpine image: kernel is not a Multiboot2 image: $kernel_path" >&2
    exit 1
fi
if ! readelf -h "$kernel_path" | awk '/Machine:.*X86-64/ { found = 1 } END { exit !found }'; then
    echo "alpine image: kernel is not an x86_64 ELF: $kernel_path" >&2
    exit 1
fi
if ! unshare --map-root-user --map-auto --mount --propagation private true 2>/dev/null; then
    echo "alpine image: subordinate UID/GID mappings or user namespaces are unavailable" >&2
    exit 1
fi
grub_platform_dir=/usr/lib/grub/i386-pc
if [ ! -d "$grub_platform_dir" ]; then
    echo "alpine image: missing BIOS GRUB platform directory: $grub_platform_dir" >&2
    exit 1
fi

staging_dir=$(mktemp -d "$build_tmp/hitoshizuku-alpine.XXXXXX")
trap 'rm -rf "$staging_dir"' EXIT HUP INT TERM
staged_root=$staging_dir/root
staged_image=$staging_dir/alpine-x86_64.img
partition_path=$staging_dir/rootfs.partition
exported_rootfs=$staging_dir/exported-rootfs
iso_stage=$staging_dir/iso
staged_iso=$staging_dir/alpine-boot.iso
mkdir -p "$staged_root"

# A full subordinate-ID map lets Alpine's normal apk/chroot path retain every
# package uid, gid, setuid and setgid bit without root or a loop mount. Keep
# mke2fs in the same namespace so those mapped IDs become their intended
# numeric values in ext4.
unshare --map-root-user --map-auto --mount --propagation private -- \
    sh -eu -s -- \
    "$staged_root" "$archive_path" "$alpine_mirror" "$alpine_branch" \
    "$packages" "$staged_image" "$partition_path" "$image_bytes" \
    "$partitioned" "$root_device" "$console_device" "$nameservers" "$exported_rootfs" \
    "$repo_root/scripts/alpine-console" <<'POPULATE'
root=$1
archive=$2
mirror=$3
branch=$4
packages=$5
image_path=$6
partition_path=$7
image_bytes=$8
partitioned=$9
root_device=${10}
console_device=${11}
nameservers=${12}
exported_rootfs=${13}
console_template=${14}

tar --extract --file "$archive" --gzip --directory "$root" --numeric-owner
mkdir -p "$root/etc/apk" "$root/tmp"
chmod 01777 "$root/tmp"
printf '%s/v%s/main\n%s/v%s/community\n' "$mirror" "$branch" "$mirror" "$branch" \
    >"$root/etc/apk/repositories"
: >"$root/etc/resolv.conf"
for nameserver in $nameservers; do
    printf 'nameserver %s\n' "$nameserver" >>"$root/etc/resolv.conf"
done

if [ ! -x "$root/sbin/apk" ]; then
    echo "alpine image: minirootfs has no executable /sbin/apk" >&2
    exit 1
fi
run_in_chroot() {
    chroot "$root" /bin/sh -c 'export TMPDIR=/tmp; exec "$@"' sh "$@"
}
set -- $packages
run_in_chroot /sbin/apk --no-cache add "$@"

if ! run_in_chroot test -x /sbin/init; then
    echo "alpine image: installed root has no executable /sbin/init" >&2
    exit 1
fi
cp --remove-destination "$console_template" "$root/sbin/hitoshizuku-console"
chmod 0755 "$root/sbin/hitoshizuku-console"
# BusyBox init runs each inittab action in a fresh child.  Keep the OpenRC
# service marker explicit across those transitions: the kernel supplies the
# native PID 1, while OpenRC still uses this marker to authorize init scripts.
cat >"$root/sbin/hitoshizuku-openrc" <<'OPENRC'
#!/bin/busybox sh
set -eu
mkdir -p /run/openrc
touch /run/openrc/softlevel
exec /sbin/openrc "$@"
OPENRC
chmod 0755 "$root/sbin/hitoshizuku-openrc"
mkdir -p "$root/etc/network" "$root/run" "$root/tmp" "$root/root" "$root/proc"
chmod 01777 "$root/tmp"
# The image is already checked by e2fsck while it is assembled.  Keep
# Alpine's standard fastboot marker so an unclean QEMU power-off does not make
# the next PID 1 run block on a forced fsck before OpenRC reaches default.
: >"$root/fastboot"
printf 'hitoshizuku-alpine\n' >"$root/etc/hostname"
printf '127.0.0.1 localhost localhost.localdomain\n::1 localhost localhost.localdomain\n127.0.1.1 hitoshizuku\n' >"$root/etc/hosts"
# The kernel network runtime owns physical interface autoconfiguration.  Keep
# ifupdown-ng responsible for loopback only; starting a second DHCP client here
# races the kernel client for the single QEMU lease and obscures the real state.
printf 'auto lo\niface lo inet loopback\n' \
    >"$root/etc/network/interfaces"
{
    printf '# Root is selected by the kernel command line.\n'
    # The kernel installs sysfs/devpts/mqueue/devtmpfs before init and OpenRC
    # mounts procfs/run in sysinit.  Keep those mounts out of mount -a: the
    # native VFS intentionally permits a second mount at a mountpoint, which
    # would hide /run/openrc/softlevel and make every service look unbooted.
    # Keep root in fstab for fsck/root metadata, but mark it noauto so
    # localmount does not stack a second extfs instance over the live root.
    printf '%s      /      ext4    rw,relatime,noauto   0 1\n' "$root_device"
} >"$root/etc/fstab"
printf 'Hitoshizuku Alpine development image.\n' >"$root/etc/motd"
printf 'export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\nexport PS1="\\u@\\h:\\w\\$ "\n' \
    >"$root/etc/profile"
cat >"$root/etc/inittab" <<'INITTAB'
::sysinit:/sbin/hitoshizuku-openrc sysinit
::sysinit:/sbin/hitoshizuku-openrc boot
::wait:/sbin/hitoshizuku-openrc default
uart0::respawn:/sbin/getty -L -n -l /sbin/hitoshizuku-console 115200 uart0 vt100
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
INITTAB
if [ -f "$root/etc/shadow" ]; then
    sed -i 's#^root:[^:]*:#root:!:#' "$root/etc/shadow"
fi
printf 'rootfs=%s\nconsole=%s\n' "$root_device" "$console_device" \
    >"$root/etc/hitoshizuku-boot.conf"
awk -F: '
    $1 == "P" { package = $2 }
    $1 == "V" && package != "" { print package "=" $2; package = "" }
' "$root/lib/apk/db/installed" | sort >"$root/etc/hitoshizuku-packages"

enable_service() {
    service=$1
    runlevel=$2
    if [ -x "$root/etc/init.d/$service" ]; then
        run_in_chroot /sbin/rc-update add "$service" "$runlevel"
    fi
}
enable_service networking boot
enable_service sshd default

unreadable=$(find "$root" -xdev -type f ! -readable -print -quit)
if [ -n "$unreadable" ]; then
    echo "alpine image: cannot read staged file: $unreadable" >&2
    exit 1
fi

if [ "$partitioned" = 1 ]; then
    start_sector=2048
    total_sectors=$((image_bytes / 512))
    part_sectors=$((total_sectors - start_sector - 4096))
    part_sectors=$((part_sectors / 2048 * 2048))
    if [ "$part_sectors" -le 0 ]; then
        echo "alpine image: image is too small for a GPT partition" >&2
        exit 1
    fi
    part_bytes=$((part_sectors * 512))
    truncate -s "$image_bytes" "$image_path"
    printf 'label: gpt\nunit: sectors\n\nstart=%s, size=%s, type=linux\n' \
        "$start_sector" "$part_sectors" | sfdisk --no-reread "$image_path" >/dev/null
    truncate -s "$part_bytes" "$partition_path"
    mkfs.ext4 -q -F -b 4096 -L alpine-root \
        -O '^64bit,^metadata_csum,^metadata_csum_seed,^orphan_file,^fast_commit' \
        -E lazy_itable_init=0,lazy_journal_init=0,no_copy_xattrs,root_owner=0:0 \
        -d "$root" "$partition_path"
    e2fsck -fn "$partition_path" >/dev/null
    mkdir -p "$exported_rootfs"
    # This host-visible copy retains the user-namespace ID mapping and is only
    # for inspection. The ext4 image above is the authoritative rootfs.
    cp -a "$root"/. "$exported_rootfs"/
    dd if="$partition_path" of="$image_path" bs=512 seek="$start_sector" \
        conv=notrunc status=none
else
    truncate -s "$image_bytes" "$image_path"
    mkfs.ext4 -q -F -b 4096 -L alpine-root \
        -O '^64bit,^metadata_csum,^metadata_csum_seed,^orphan_file,^fast_commit' \
        -E lazy_itable_init=0,lazy_journal_init=0,no_copy_xattrs,root_owner=0:0 \
        -d "$root" "$image_path"
    e2fsck -fn "$image_path" >/dev/null
    mkdir -p "$exported_rootfs"
    cp -a "$root"/. "$exported_rootfs"/
fi
POPULATE

mkdir -p "$iso_stage/boot/grub"
cp "$kernel_path" "$iso_stage/boot/kernel.elf"
cat >"$iso_stage/boot/grub/grub.cfg" <<EOF
set timeout=0
set timeout_style=hidden
serial --unit=0 --speed=115200 --word=8 --parity=no --stop=1
terminal_input serial
terminal_output serial
menuentry "Hitoshizuku Alpine x86_64" {
    multiboot2 /boot/kernel.elf console=${console_device} root=${root_device} rw init=/sbin/init
    boot
}
EOF
grub2-mkrescue -d "$grub_platform_dir" -o "$staged_iso" "$iso_stage" >/dev/null

# Publish only complete artifacts. Output paths were constrained to build/
# above before any recursive removal is allowed.
rm -rf "$rootfs_dir"
mv "$exported_rootfs" "$rootfs_dir"
mv -f "$staged_image" "$image_path"
mv -f "$staged_iso" "$iso_path"

echo "alpine image: inspection tree=$rootfs_dir"
echo "alpine image: disk=$image_path"
echo "alpine image: boot ISO=$iso_path"
echo "alpine image: root device=$root_device console=$console_device"
echo "alpine image: run scripts/run-alpine-x86_64.sh"
