#!/bin/sh
set -eu

usage() {
    cat >&2 <<'EOF'
用法:
  build.sh --workload <clock-read|stream-write|stream-write-1|stream-write-64|
             stream-write-256|heap-small|heap-batch|map-large|page-touch> --output <dir>
           [--mode <warm|cold>] [--samples <count>] [--rounds <count>]
           [--warmup <count>] [--cycles <count>] [--counter-hz <hz>]
           [--busybox <path>]
EOF
    exit 2
}

workload=
output=
mode=warm
samples=1000
rounds=5
warmup=1000
cycles=3
counter_hz=10000000
busybox=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --workload) [ "$#" -ge 2 ] || usage; workload=$2; shift 2 ;;
        --output) [ "$#" -ge 2 ] || usage; output=$2; shift 2 ;;
        --mode) [ "$#" -ge 2 ] || usage; mode=$2; shift 2 ;;
        --samples) [ "$#" -ge 2 ] || usage; samples=$2; shift 2 ;;
        --rounds) [ "$#" -ge 2 ] || usage; rounds=$2; shift 2 ;;
        --warmup) [ "$#" -ge 2 ] || usage; warmup=$2; shift 2 ;;
        --cycles) [ "$#" -ge 2 ] || usage; cycles=$2; shift 2 ;;
        --counter-hz) [ "$#" -ge 2 ] || usage; counter_hz=$2; shift 2 ;;
        --busybox) [ "$#" -ge 2 ] || usage; busybox=$2; shift 2 ;;
        *) usage ;;
    esac
done
case "$workload" in
    clock-read|stream-write|stream-write-1|stream-write-64|stream-write-256|\
        heap-small|heap-batch|map-large|page-touch) ;;
    *) usage ;;
esac
case "$mode" in warm|cold) ;; *) usage ;; esac
[ -n "$output" ] || usage
for value in "$samples" "$rounds" "$cycles" "$counter_hz"; do
    case "$value" in ''|*[!0-9]*|0) usage ;; esac
done
case "$warmup" in ''|*[!0-9]*) usage ;; esac
if [ "$mode" = cold ]; then
    [ "$samples" = 1 ] && [ "$rounds" = 1 ] && [ "$warmup" = 0 ] || usage
fi

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
repo=$(CDPATH= cd -- "$script_dir/../../../.." && pwd)
native_root="$repo/native"
case "$output" in
    /*) output_path=$output ;;
    *) output_path="$PWD/$output" ;;
esac
output_parent=$(dirname "$output_path")
mkdir -p "$output_parent"
output=$(CDPATH= cd -- "$output_parent" && pwd)/$(basename "$output_path")
mkdir -p "$output"

if [ -z "$busybox" ]; then
    busybox="$repo/build/riscv64/busybox-rootfs/bin/busybox"
fi
[ -f "$busybox" ] || { echo "缺少 busybox: $busybox" >&2; exit 3; }

riscv_linux_cc=${RISCV_LINUX_CC:-riscv64-linux-musl-gcc}
riscv_clang=${CLANG:-clang}
native_make=${MAKE:-make}
command -v "$riscv_linux_cc" >/dev/null 2>&1 || {
    echo "UNAVAILABLE reason=linux_compiler_missing" >&2
    exit 3
}
command -v "$riscv_clang" >/dev/null 2>&1 || {
    echo "UNAVAILABLE reason=clang_missing" >&2
    exit 3
}
command -v "$native_make" >/dev/null 2>&1 || {
    echo "UNAVAILABLE reason=make_missing" >&2
    exit 3
}
command -v sha256sum >/dev/null 2>&1 || {
    echo "UNAVAILABLE reason=sha256sum_missing" >&2
    exit 3
}

native_output="$output/native-build"
binding="$native_output/include/mygo_program.h"
mkdir -p "$native_output/include" "$native_output/obj"
soyo_ld="$repo/tools/soyo-linker/target/x86_64-unknown-linux-gnu/release/soyo-ld"
if [ -n "${SOYO_LD:-}" ]; then
    soyo_ld=$SOYO_LD
elif ! "$soyo_ld" --version >/dev/null 2>&1; then
    soyo_ld="$output/soyo-ld"
    cat >"$soyo_ld" <<EOF
#!/bin/sh
exec cargo run --manifest-path "$repo/tools/soyo-linker/Cargo.toml" \\
    --target x86_64-unknown-linux-gnu --release --bin soyo-ld -- "\$@"
EOF
    chmod 0755 "$soyo_ld"
fi

mrt_objects="
$native_output/obj/mrt-entry.o
$native_output/obj/mrt-start.o
$native_output/obj/mrt-start-info.o
$native_output/obj/mrt-lifecycle.o
$native_output/obj/mrt-process.o
$native_output/obj/mrt-component.o
$native_output/obj/mrt-entry-c.o"
ranalib_objects="
$native_output/obj/ranalib-assert.o
$native_output/obj/ranalib-ctype.o
$native_output/obj/ranalib-errno.o
$native_output/obj/ranalib-exit.o
$native_output/obj/ranalib-tlsf.o
$native_output/obj/ranalib-heap.o
$native_output/obj/ranalib-inttypes.o
$native_output/obj/ranalib-locale.o
$native_output/obj/ranalib-stdlib.o
$native_output/obj/ranalib-string.o
$native_output/obj/ranalib-stdio.o
$native_output/obj/ranalib-time.o
$native_output/obj/ranalib-threads.o"
native_heap_objects="
$native_output/obj/ranalib-errno.o
$native_output/obj/ranalib-tlsf.o
$native_output/obj/ranalib-heap.o
$native_output/obj/ranalib-string.o"

build_native_runtime() {
    manifest=$1
    include_ranalib=$2
    "$soyo_ld" --target riscv64 --manifest "$manifest" --emit-c-header "$binding"
    # 目标名在 native/Makefile 中是绝对路径，显式请求可避免构建无关示例。
    if [ "$include_ranalib" = yes ]; then
        # shellcheck disable=SC2086
        "$native_make" -C "$native_root" ARCH=riscv64 OUTPUT="$native_output" \
            MANIFEST="$manifest" \
            SOYO_LD="$soyo_ld" $mrt_objects $ranalib_objects
    elif [ "$include_ranalib" = heap ]; then
        # shellcheck disable=SC2086
        "$native_make" -C "$native_root" ARCH=riscv64 OUTPUT="$native_output" \
            MANIFEST="$manifest" \
            SOYO_LD="$soyo_ld" $mrt_objects $native_heap_objects
    else
        # shellcheck disable=SC2086
        "$native_make" -C "$native_root" ARCH=riscv64 OUTPUT="$native_output" \
            MANIFEST="$manifest" \
            SOYO_LD="$soyo_ld" $mrt_objects
    fi
}

pack_initramfs() {
    image=$1
    program=$2
    platform=$3
    boot=$4
    image_mode=$5
    tmp="$output/.root-$platform-$boot"
    rm -rf "$tmp"
    if [ "$platform" = linux ]; then
        mkdir -p "$tmp/bin" "$tmp/dev" "$tmp/proc" "$tmp/sys" "$tmp/tmp"
        cp "$busybox" "$tmp/bin/busybox"
        ln -s busybox "$tmp/bin/sh"
    else
        mygo_root="$repo/build/riscv64/compat-rootfs"
        [ -f "$mygo_root/lib/elm/modules.manifest" ] || {
            echo "UNAVAILABLE reason=mygo_compat_rootfs_missing" >&2
            exit 3
        }
        mkdir -p "$tmp"
        cp -a "$mygo_root/." "$tmp/"
    fi
    cp "$program" "$tmp/bin/bench"
    source_sha=$(sha256sum "$program" | awk '{print $1}')
    installed_sha=$(sha256sum "$tmp/bin/bench" | awk '{print $1}')
    [ "$source_sha" = "$installed_sha" ] || {
        echo "安装后的 benchmark hash 不一致" >&2
        exit 1
    }
    cat >"$tmp/init" <<EOF
#!/bin/sh
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
echo "[bench-init] system=$platform workload=$workload mode=$image_mode boot=$boot"
/bin/bench "$platform" "$boot" "$image_mode"
status=\$?
/bin/busybox poweroff -f 2>/dev/null || true
exit \$status
EOF
    chmod 0755 "$tmp/init" "$tmp/bin/bench"
    (cd "$tmp" && find . -print | cpio -o -H newc --quiet >"$image")
    rm -rf "$tmp"
}

build_matrix_workload() {
    case "$workload" in
        clock-read) workload_id=1 ;;
        stream-write) workload_id=2 ;;
        stream-write-1) workload_id=3 ;;
        stream-write-64) workload_id=4 ;;
        stream-write-256) workload_id=5 ;;
        heap-small) workload_id=6 ;;
        heap-batch) workload_id=7 ;;
        map-large) workload_id=8 ;;
        page-touch) workload_id=9 ;;
    esac
    posix_elf="$output/bench-posix-$workload-$mode"
    native_soyo="$output/bench-native-$workload-$mode.soyo"
    posix_core="$output/bench-core-posix-$workload-$mode.o"
    native_core="$output/bench-core-native-$workload-$mode.o"
    native_platform="$output/platform-native-$workload-$mode.o"
    native_printf_stub="$native_output/obj/native-printf-stub.o"

    "$riscv_linux_cc" -static -O2 -std=c11 -Wall -Wextra -Werror \
        -DBENCH_WORKLOAD="$workload_id" -DBENCH_MODE="$([ "$mode" = cold ] && printf 2 || printf 1)" \
        -DBENCH_SAMPLES="$samples" -DBENCH_ROUNDS="$rounds" -DBENCH_WARMUP="$warmup" \
        -DBENCH_COUNTER_HZ="$counter_hz" -I"$script_dir" \
        "$script_dir/bench-core.c" "$script_dir/platform-posix.c" -o "$posix_elf"

    manifest="$script_dir/program.json"
    runtime_kind=no
    case "$workload" in
        heap-small|heap-batch) runtime_kind=heap ;;
    esac
    build_native_runtime "$manifest" "$runtime_kind"
    mode_id=1
    [ "$mode" = cold ] && mode_id=2
    common_flags="--target=riscv64-unknown-none-elf -std=c11 -ffreestanding -fno-builtin
-fno-pic -fno-pie -fno-stack-protector -fno-asynchronous-unwind-tables
-fno-unwind-tables -fvisibility=hidden -Wall -Wextra -Werror -O2
-I$native_output/include -I$native_root/include -I$script_dir
-mno-relax -msmall-data-limit=0 -mcmodel=medany
-DBENCH_WORKLOAD=$workload_id -DBENCH_MODE=$mode_id -DBENCH_SAMPLES=$samples
-DBENCH_ROUNDS=$rounds -DBENCH_WARMUP=$warmup -DBENCH_COUNTER_HZ=$counter_hz"
    # shellcheck disable=SC2086
    "$riscv_clang" $common_flags -c "$script_dir/bench-core.c" -o "$native_core"
    # shellcheck disable=SC2086
    "$riscv_clang" $common_flags -c "$script_dir/platform-native.c" -o "$native_platform"
    # shellcheck disable=SC2086
    link_objects="$mrt_objects"
    case "$runtime_kind" in
        heap)
            "$riscv_clang" $common_flags -c "$script_dir/native-printf-stub.c" \
                -o "$native_printf_stub"
            link_objects="$link_objects $native_heap_objects $native_printf_stub"
            ;;
    esac
    # shellcheck disable=SC2086
    "$soyo_ld" --target riscv64 --manifest "$manifest" -o "$native_soyo" \
        $link_objects "$native_core" "$native_platform"

    posix_sha=$(sha256sum "$posix_elf" | awk '{print $1}')
    native_sha=$(sha256sum "$native_soyo" | awk '{print $1}')
    {
        printf 'format=platform-comparison-build-1\n'
        printf 'workload=%s\nmode=%s\nsamples=%s\nrounds=%s\nwarmup=%s\ncycles=%s\ncounter_hz=%s\n' \
            "$workload" "$mode" "$samples" "$rounds" "$warmup" "$cycles" "$counter_hz"
        printf 'posix_elf=%s\nposix_elf_sha256=%s\n' "$posix_elf" "$posix_sha"
        printf 'native_soyo=%s\nnative_soyo_sha256=%s\n' "$native_soyo" "$native_sha"
    } >"$output/build.meta"

    boot=0
    while [ "$boot" -lt "$cycles" ]; do
        pack_initramfs "$output/linux-$workload-$mode-boot-$boot.cpio" \
            "$posix_elf" linux "$boot" "$mode"
        pack_initramfs "$output/mygo-tomori-$workload-$mode-boot-$boot.cpio" \
            "$posix_elf" mygo-tomori "$boot" "$mode"
        pack_initramfs "$output/mygo-native-$workload-$mode-boot-$boot.cpio" \
            "$native_soyo" mygo-native "$boot" "$mode"
        boot=$((boot + 1))
    done
}

build_matrix_workload

cat "$output/build.meta" 2>/dev/null || true
