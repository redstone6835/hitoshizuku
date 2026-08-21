#!/bin/sh
# 在完全相同的 QEMU RISC-V64 配置下比较匿名页缺页路径。
set -eu

usage() {
    echo "usage: $0 <trace|timing> [mygo|linux|both]" >&2
    exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
mode=$1
systems=${2:-both}
case "$mode" in trace|timing) ;; *) usage ;; esac
case "$systems" in mygo|linux|both) ;; *) usage ;; esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
image=${MM_BENCH_CONTAINER:-}
[ -n "$image" ] || {
    echo "MM_BENCH_CONTAINER must name a build image" >&2
    exit 2
}
smp=${MM_BENCH_SMP:-1}
memory=${MM_BENCH_MEMORY:-1G}
accel=${MM_BENCH_ACCEL:-tcg,thread=single}
timeout_seconds=${MM_BENCH_TIMEOUT:-600}
benchmark_case=${MM_BENCH_CASE:-anon-write}
pages=${MM_BENCH_PAGES:-1}
threads=${MM_BENCH_THREADS:-1}
repeats=${MM_BENCH_REPEATS:-1}
max_instructions=${MM_BENCH_MAX_INSTRUCTIONS:-1000000}

for pair in \
    "MM_BENCH_SMP:$smp" \
    "MM_BENCH_TIMEOUT:$timeout_seconds" \
    "MM_BENCH_PAGES:$pages" \
    "MM_BENCH_THREADS:$threads" \
    "MM_BENCH_REPEATS:$repeats" \
    "MM_BENCH_MAX_INSTRUCTIONS:$max_instructions"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be an integer" >&2; exit 2 ;; esac
done
[ "$smp" -gt 0 ] && [ "$timeout_seconds" -gt 0 ] && [ "$pages" -gt 0 ] && \
    [ "$threads" -gt 0 ] && [ "$repeats" -gt 0 ] || usage
[ "$threads" -le "$pages" ] || { echo "MM_BENCH_THREADS cannot exceed pages" >&2; exit 2; }
[ "$max_instructions" -ge 1 ] && [ "$max_instructions" -le 10000000 ] || usage
case "$benchmark_case" in anon-read|anon-write|resident-write) ;; *) usage ;; esac
if [ "$mode" = trace ]; then
    [ "$systems" = both ] || { echo "trace mode requires both systems" >&2; exit 2; }
    [ "$smp" -eq 1 ] && [ "$threads" -eq 1 ] && [ "$repeats" -eq 1 ] || {
        echo "trace mode requires smp=threads=repeats=1" >&2
        exit 2
    }
    [ "$benchmark_case" != resident-write ] || {
        echo "trace mode requires a faulting benchmark case" >&2
        exit 2
    }
fi

absolute_path() {
    candidate=$1
    case "$candidate" in /*) ;; *) candidate=$root/$candidate ;; esac
    directory=$(CDPATH= cd -- "$(dirname "$candidate")" && pwd -P) || exit 1
    printf '%s/%s\n' "$directory" "$(basename "$candidate")"
}

repo_relative() {
    case "$1" in
        "$root"/*) printf '%s\n' "${1#"$root"/}" ;;
        *) echo "artifact must be inside the repository: $1" >&2; exit 1 ;;
    esac
}

mygo_kernel=$(absolute_path "${MM_BENCH_MYGO_KERNEL:-$root/kernel-rv}")
mygo_map=$(absolute_path "${MM_BENCH_MYGO_MAP:-$root/build/riscv64/kernel.map}")
mygo_manifest=$(absolute_path "${MM_BENCH_MYGO_MANIFEST:-${mygo_map}.manifest}")
linux_image=$(absolute_path \
    "${MM_BENCH_LINUX_IMAGE:-$root/build/linux-riscv64/arch/riscv/boot/Image}")
linux_vmlinux=$(absolute_path \
    "${MM_BENCH_LINUX_VMLINUX:-$root/build/linux-riscv64/vmlinux}")
linux_map=$(absolute_path "${MM_BENCH_LINUX_MAP:-$root/build/linux-riscv64/System.map}")
linux_manifest=$(absolute_path "${MM_BENCH_LINUX_MANIFEST:-${linux_map}.manifest}")
initramfs=$(absolute_path \
    "${MM_BENCH_INITRAMFS:-$root/build/riscv64/compat-initramfs.cpio}")
benchmark_elf=$(absolute_path \
    "${MM_BENCH_ELF:-$root/build/riscv64/mm-bench/mm-fault-bench.elf}")

for artifact in "$mygo_kernel" "$mygo_map" "$mygo_manifest" \
    "$linux_image" "$linux_vmlinux" "$linux_map" "$linux_manifest" \
    "$initramfs" "$benchmark_elf"
do
    [ -f "$artifact" ] || { echo "missing MM benchmark artifact: $artifact" >&2; exit 1; }
    repo_relative "$artifact" >/dev/null
done

run_id=$(date -u +%Y%m%dT%H%M%SZ)-$mode-$benchmark_case-$$
run_root=${MM_BENCH_RUN_ROOT:-$root/build/mm-bench-runs}
output=$run_root/$run_id
case "$output" in "$root"/*) ;; *) echo "run output must be inside the repository" >&2; exit 2 ;; esac
output_relative=${output#"$root"/}
host_uid=$(id -u)
host_gid=$(id -g)
docker run --rm -v "$root":/work -w /work "$image" sh -eu -c '
    mkdir -p "$1/artifacts"
    chown "$2:$3" "$1" "$1/artifacts"
' create-run-directory "/work/$output_relative" "$host_uid" "$host_gid"

snapshot() {
    cp --reflink=auto -- "$1" "$output/artifacts/$2"
}
snapshot "$mygo_kernel" mygo.kernel
snapshot "$mygo_map" mygo.kernel.map
snapshot "$mygo_manifest" mygo.kernel.map.manifest
snapshot "$linux_image" linux.Image
snapshot "$linux_vmlinux" linux.vmlinux
snapshot "$linux_map" linux.System.map
snapshot "$linux_manifest" linux.System.map.manifest
snapshot "$initramfs" compat-initramfs.cpio
snapshot "$benchmark_elf" mm-fault-bench.elf

artifacts=$output/artifacts
profile_start_pc=
profile_stop_pc=
plugin=
if [ "$mode" = trace ]; then
    symbols=$(docker run --rm -v "$root":/work -w /work "$image" \
        riscv64-linux-gnu-nm -n "/work/${artifacts#"$root"/}/mm-fault-bench.elf")
    profile_start_pc=$(printf '%s\n' "$symbols" |
        awk '$3 == "mm_profile_start" { print "0x" $1 }')
    profile_stop_pc=$(printf '%s\n' "$symbols" |
        awk '$3 == "mm_profile_stop" { print "0x" $1 }')
    [ "$(printf '%s\n' "$profile_start_pc" | awk 'NF { n++ } END { print n + 0 }')" -eq 1 ] &&
        [ "$(printf '%s\n' "$profile_stop_pc" | awk 'NF { n++ } END { print n + 0 }')" -eq 1 ] &&
        [ "$profile_start_pc" != "$profile_stop_pc" ] || {
        echo "MM profile markers must be distinct, unique symbols" >&2
        exit 1
    }
    plugin=$output/mygo-tcg-instruction-trace.so
    plugin_relative=${plugin#"$root"/}
    docker run --rm -v "$root":/work -w /work "$image" bash -euxo pipefail -c '
        cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared \
            -I/opt/qemu-bin-10.0.2/include $(pkg-config --cflags glib-2.0) \
            tools/qemu-plugins/mygo-tcg-instruction-trace.c -o "$1"
    ' build-plugin "/work/$plugin_relative"
fi

{
    printf 'schema=mygo.mm-bench-run.v1\n'
    printf 'mode=%s\nsystems=%s\ncase=%s\npages=%s\nthreads=%s\nrepeats=%s\n' \
        "$mode" "$systems" "$benchmark_case" "$pages" "$threads" "$repeats"
    printf 'smp=%s\nmemory=%s\naccel=%s\ncontainer=%s\n' "$smp" "$memory" "$accel" "$image"
    printf 'start_pc=%s\nstop_pc=%s\n' "${profile_start_pc:-none}" "${profile_stop_pc:-none}"
    for artifact in "$artifacts"/*; do
        printf 'artifact_%s_sha256=%s\n' "$(basename "$artifact" | tr '.-' '__')" \
            "$(sha256sum "$artifact" | awk '{print $1}')"
    done
} >"$output/run.metadata.env"

# 验证 initramfs 内实际执行的是同一 ELF 的 stripped 副本。
docker run --rm -v "$root":/work -w /work "$image" bash -eu -c '
    run=$1
    temporary=$(mktemp -d)
    trap '\''rm -rf "$temporary"'\'' EXIT
    cd "$temporary"
    cpio -i --quiet --to-stdout bin/mm-fault-bench \
        <"/work/$run/artifacts/compat-initramfs.cpio" >installed
    cp "/work/$run/artifacts/mm-fault-bench.elf" expected
    riscv64-linux-musl-strip expected
    cmp expected installed
' validate-initramfs "${output#"$root"/}"

run_system() {
    system=$1
    serial=$output/$system.serial.log
    trace=$output/$system.instruction-trace.txt
    case "$system" in
        mygo) kernel=$artifacts/mygo.kernel ;;
        linux) kernel=$artifacts/linux.Image ;;
        *) return 2 ;;
    esac
    kernel_relative=${kernel#"$root"/}
    initramfs_relative=${artifacts#"$root"/}/compat-initramfs.cpio
    serial_relative=${serial#"$root"/}
    trace_relative=${trace#"$root"/}
    plugin_relative=${plugin#"$root"/}

    echo "[mm-bench] 运行 $system：mode=$mode case=$benchmark_case pages=$pages threads=$threads"
    docker run --rm -v "$root":/work -w /work "$image" bash -c '
        set -eu
        system=$1; mode=$2; kernel=$3; initramfs=$4; serial=$5; trace=$6; plugin=$7
        smp=$8; memory=$9; shift 9
        accel=$1; timeout_seconds=$2; benchmark_case=$3; pages=$4; threads=$5
        repeats=$6; start_pc=$7; stop_pc=$8; max_instructions=$9
        set -- qemu-system-riscv64 -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$accel" -bios default -kernel "$kernel" -initrd "$initramfs" \
            -m "$memory" -smp "$smp" -nographic -no-reboot -rtc base=utc
        append="mm_bench_case=$benchmark_case mm_bench_pages=$pages mm_bench_threads=$threads mm_bench_repeats=$repeats"
        if [ "$system" = linux ]; then
            append="console=ttyS0 panic=-1 rdinit=/sbin/init $append"
        fi
        set -- "$@" -append "$append"
        if [ "$mode" = trace ]; then
            set -- "$@" -plugin \
                "file=$plugin,output=$trace,max_instructions=$max_instructions,start_pc=$start_pc,stop_pc=$stop_pc"
        fi
        timeout -k 10 "$timeout_seconds" "$@" >"$serial" 2>&1
    ' run-qemu "$system" "$mode" "/work/$kernel_relative" "/work/$initramfs_relative" \
        "/work/$serial_relative" "/work/$trace_relative" "/work/$plugin_relative" \
        "$smp" "$memory" "$accel" "$timeout_seconds" "$benchmark_case" "$pages" \
        "$threads" "$repeats" "$profile_start_pc" "$profile_stop_pc" "$max_instructions"

    [ "$(grep -c '^MM_FAULT_BENCH ' "$serial" || true)" -eq 1 ] &&
        [ "$(grep -c '^MM_FAULT_RESULT ' "$serial" || true)" -eq "$repeats" ] &&
        [ "$(grep -c '^MM_FAULT_BENCH_DONE status=0 ' "$serial" || true)" -eq 1 ] &&
        [ "$(grep -c '^MM_FAULT_GUEST_DONE status=0' "$serial" || true)" -eq 1 ] || {
        echo "invalid $system MM benchmark serial result" >&2
        tail -80 "$serial" >&2
        exit 1
    }
    if [ "$mode" = trace ]; then
        [ -s "$trace" ] || { echo "missing $system instruction trace" >&2; exit 1; }
    fi
    sed -n -e '/^MM_FAULT_BENCH /p' -e '/^MM_FAULT_RESULT /p' \
        -e '/^MM_FAULT_BENCH_DONE /p' -e '/^MM_FAULT_GUEST_DONE /p' "$serial"
}

case "$systems" in
    mygo) run_system mygo ;;
    linux) run_system linux ;;
    both) run_system mygo; run_system linux ;;
esac

if [ "$mode" = trace ]; then
    mygo_trace=${output#"$root"/}/mygo.instruction-trace.txt
    linux_trace=${output#"$root"/}/linux.instruction-trace.txt
    artifact_relative=${artifacts#"$root"/}
    output_relative=${output#"$root"/}
    docker run --rm -v "$root":/work -w /work "$image" \
        python3 scripts/syscall-instruction-compare.py \
        --path-kind page-fault \
        --allow-linux-alternatives \
        --mygo-trace "/work/$mygo_trace" \
        --linux-trace "/work/$linux_trace" \
        --benchmark-elf "/work/$artifact_relative/mm-fault-bench.elf" \
        --mygo-kernel "/work/$artifact_relative/mygo.kernel" \
        --linux-vmlinux "/work/$artifact_relative/linux.vmlinux" \
        --mygo-output "/work/$output_relative/mygo.instruction-sequence.tsv" \
        --linux-output "/work/$output_relative/linux.instruction-sequence.tsv"
fi

echo "[mm-bench] 输出目录：$output"
