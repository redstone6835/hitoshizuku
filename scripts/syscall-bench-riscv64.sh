#!/bin/sh
# 在完全相同的 QEMU RISC-V64 配置下运行 Hitoshizuku/Linux syscall 基准。
set -eu

usage() {
    echo "usage: $0 <timing|profile|trace> [mygo|linux|both]" >&2
    exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 2 ] || usage
mode=$1
systems=${2:-both}
case "$mode" in timing|profile|trace) ;; *) usage ;; esac
case "$systems" in mygo|linux|both) ;; *) usage ;; esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
image=${SYSCALL_BENCH_CONTAINER:-}
[ -n "$image" ] || {
    echo "SYSCALL_BENCH_CONTAINER must name a build image" >&2
    exit 2
}
smp=${SYSCALL_BENCH_SMP:-1}
memory=${SYSCALL_BENCH_MEMORY:-1G}
accel=${SYSCALL_BENCH_ACCEL:-tcg,thread=single}
timeout_seconds=${SYSCALL_BENCH_TIMEOUT:-600}
table_bits=${SYSCALL_BENCH_TABLE_BITS:-20}
max_instructions=${SYSCALL_BENCH_MAX_INSTRUCTIONS:-100000}
case "$mode" in
    profile)
        iterations=${SYSCALL_BENCH_ITERATIONS:-2000000}
        repeats=${SYSCALL_BENCH_REPEATS:-1}
        benchmark_case=${SYSCALL_BENCH_CASE:-getpid}
        warmup=${SYSCALL_BENCH_WARMUP:-100000}
        ;;
    trace)
        iterations=${SYSCALL_BENCH_ITERATIONS:-1}
        repeats=${SYSCALL_BENCH_REPEATS:-1}
        benchmark_case=${SYSCALL_BENCH_CASE:-getpid}
        warmup=${SYSCALL_BENCH_WARMUP:-0}
        ;;
    timing)
        iterations=${SYSCALL_BENCH_ITERATIONS:-1000000}
        repeats=${SYSCALL_BENCH_REPEATS:-5}
        benchmark_case=${SYSCALL_BENCH_CASE:-all}
        warmup=${SYSCALL_BENCH_WARMUP:-100000}
        ;;
esac

for pair in \
    "SYSCALL_BENCH_SMP:$smp" \
    "SYSCALL_BENCH_TIMEOUT:$timeout_seconds" \
    "SYSCALL_BENCH_TABLE_BITS:$table_bits" \
    "SYSCALL_BENCH_MAX_INSTRUCTIONS:$max_instructions" \
    "SYSCALL_BENCH_ITERATIONS:$iterations" \
    "SYSCALL_BENCH_REPEATS:$repeats" \
    "SYSCALL_BENCH_WARMUP:$warmup"
do
    name=${pair%%:*}
    value=${pair#*:}
    case "$value" in ''|*[!0-9]*) echo "$name must be an integer" >&2; exit 2 ;; esac
done
[ "$smp" -gt 0 ] && [ "$timeout_seconds" -gt 0 ] && [ "$iterations" -gt 0 ] && \
    [ "$repeats" -gt 0 ] || usage
[ "$table_bits" -ge 12 ] && [ "$table_bits" -le 23 ] || usage
[ "$max_instructions" -ge 1 ] && [ "$max_instructions" -le 10000000 ] || usage
case "$benchmark_case" in ''|*[!A-Za-z0-9_]* ) echo "invalid syscall case" >&2; exit 2 ;; esac
if [ "$mode" = profile ] || [ "$mode" = trace ]; then
    [ "$smp" -eq 1 ] || {
        echo "$mode mode requires SYSCALL_BENCH_SMP=1" >&2
        exit 2
    }
    [ "$repeats" -eq 1 ] || {
        echo "$mode mode requires SYSCALL_BENCH_REPEATS=1" >&2
        exit 2
    }
    [ "$benchmark_case" != all ] || {
        echo "$mode mode requires one syscall case, not all" >&2
        exit 2
    }
fi
if [ "$mode" = trace ]; then
    [ "$systems" = both ] || {
        echo "trace mode requires both systems" >&2
        exit 2
    }
    [ "$iterations" -eq 1 ] && [ "$warmup" -eq 0 ] || {
        echo "trace mode requires iterations=1 and warmup=0" >&2
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

mygo_kernel=$(absolute_path "${SYSCALL_BENCH_MYGO_KERNEL:-$root/kernel-rv}")
mygo_map=$(absolute_path "${SYSCALL_BENCH_MYGO_MAP:-$root/build/riscv64/kernel.map}")
mygo_manifest=$(absolute_path \
    "${SYSCALL_BENCH_MYGO_MANIFEST:-${mygo_map}.manifest}")
linux_image=$(absolute_path \
    "${SYSCALL_BENCH_LINUX_IMAGE:-$root/build/linux-riscv64/arch/riscv/boot/Image}")
linux_symbols=$(absolute_path \
    "${SYSCALL_BENCH_LINUX_SYMBOLS:-$root/build/linux-riscv64/vmlinux}")
linux_map=$(absolute_path \
    "${SYSCALL_BENCH_LINUX_MAP:-$root/build/linux-riscv64/System.map}")
linux_manifest=$(absolute_path \
    "${SYSCALL_BENCH_LINUX_MANIFEST:-${linux_map}.manifest}")
initramfs=$(absolute_path \
    "${SYSCALL_BENCH_INITRAMFS:-$root/build/riscv64/compat-initramfs.cpio}")
benchmark_symbols=$(absolute_path \
    "${SYSCALL_BENCH_ELF:-$root/build/riscv64/syscall-bench/syscall-bench.elf}")

for artifact in \
    "$mygo_kernel" "$mygo_map" "$mygo_manifest" \
    "$linux_image" "$linux_symbols" "$linux_map" "$linux_manifest" \
    "$initramfs" "$benchmark_symbols"
do
    [ -f "$artifact" ] || {
        echo "missing syscall benchmark artifact: $artifact" >&2
        exit 1
    }
    repo_relative "$artifact" >/dev/null
done

source_mygo_kernel_relative=$(repo_relative "$mygo_kernel")
source_mygo_map_relative=$(repo_relative "$mygo_map")
source_mygo_manifest_relative=$(repo_relative "$mygo_manifest")
source_linux_image_relative=$(repo_relative "$linux_image")
source_linux_symbols_relative=$(repo_relative "$linux_symbols")
source_linux_map_relative=$(repo_relative "$linux_map")
source_linux_manifest_relative=$(repo_relative "$linux_manifest")
source_initramfs_relative=$(repo_relative "$initramfs")
source_benchmark_symbols_relative=$(repo_relative "$benchmark_symbols")

run_id=$(date -u +%Y%m%dT%H%M%SZ)-$mode-$benchmark_case-$$
run_root=${SYSCALL_BENCH_RUN_ROOT:-$root/build/syscall-bench-runs}
output=$run_root/$run_id
case "$output" in "$root"/*) ;; *) echo "run output must be inside the repository" >&2; exit 2 ;; esac
output_relative=${output#"$root"/}
metadata_relative=$output_relative/run.metadata.json

artifacts_relative=$output_relative/artifacts
# QEMU、符号化和元数据只使用独立快照，后续重建不能改写历史运行的输入。
docker run --rm -v "$root":/work -w /work "$image" bash -eu -c '
    destination=$1
    shift
    mkdir -p "$destination"
    while [ "$#" -ne 0 ]; do
        cp --reflink=auto -- "$1" "$destination/$2"
        shift 2
    done
' snapshot-artifacts "/work/$artifacts_relative" \
    "/work/$source_mygo_kernel_relative" mygo.kernel \
    "/work/$source_mygo_map_relative" mygo.kernel.map \
    "/work/$source_mygo_manifest_relative" mygo.kernel.map.manifest \
    "/work/$source_linux_image_relative" linux.Image \
    "/work/$source_linux_symbols_relative" linux.vmlinux \
    "/work/$source_linux_map_relative" linux.System.map \
    "/work/$source_linux_manifest_relative" linux.System.map.manifest \
    "/work/$source_initramfs_relative" compat-initramfs.cpio \
    "/work/$source_benchmark_symbols_relative" syscall-bench.elf

mygo_kernel_relative=$artifacts_relative/mygo.kernel
mygo_map_relative=$artifacts_relative/mygo.kernel.map
mygo_manifest_relative=$artifacts_relative/mygo.kernel.map.manifest
linux_image_relative=$artifacts_relative/linux.Image
linux_symbols_relative=$artifacts_relative/linux.vmlinux
linux_map_relative=$artifacts_relative/linux.System.map
linux_manifest_relative=$artifacts_relative/linux.System.map.manifest
initramfs_relative=$artifacts_relative/compat-initramfs.cpio
benchmark_symbols_relative=$artifacts_relative/syscall-bench.elf

plugin_relative=
profile_start_pc=
profile_stop_pc=
if [ "$mode" = profile ] || [ "$mode" = trace ]; then
    if [ "$mode" = profile ]; then
        plugin_relative=$output_relative/mygo-tcg-profile.so
        plugin_source=tools/qemu-plugins/mygo-tcg-profile.c
    else
        plugin_relative=$output_relative/mygo-tcg-instruction-trace.so
        plugin_source=tools/qemu-plugins/mygo-tcg-instruction-trace.c
    fi
    profile_symbols=$(docker run --rm -v "$root":/work -w /work "$image" \
        riscv64-linux-gnu-nm -n "/work/$benchmark_symbols_relative")
    profile_start_pc=$(printf '%s\n' "$profile_symbols" |
        awk '$3 == "syscall_profile_start" { print "0x" $1 }')
    profile_stop_pc=$(printf '%s\n' "$profile_symbols" |
        awk '$3 == "syscall_profile_stop" { print "0x" $1 }')
    [ "$(printf '%s\n' "$profile_start_pc" | awk 'NF { count++ } END { print count + 0 }')" -eq 1 ] &&
        [ "$(printf '%s\n' "$profile_stop_pc" | awk 'NF { count++ } END { print count + 0 }')" -eq 1 ] || {
        echo "syscall profile markers must each have exactly one symbol" >&2
        exit 1
    }
    [ "$profile_start_pc" != "$profile_stop_pc" ] || {
        echo "syscall profile markers must be distinct" >&2
        exit 1
    }
    docker run --rm -v "$root":/work -w /work "$image" bash -euxo pipefail -c '
        cc -std=c11 -O2 -Wall -Wextra -Werror -fPIC -shared \
            -I/opt/qemu-bin-10.0.2/include \
            $(pkg-config --cflags glib-2.0) \
            "$1" -o "$2"
    ' build-plugin "/work/$plugin_source" "/work/$plugin_relative"
fi

set -- docker run --rm -v "$root":/work -w /work "$image" \
    python3 scripts/syscall-bench-report.py \
    --mode "$mode" --systems "$systems" --repo-root /work \
    --write-metadata "/work/$metadata_relative" \
    --mygo-kernel "/work/$mygo_kernel_relative" \
    --mygo-map "/work/$mygo_map_relative" \
    --mygo-manifest "/work/$mygo_manifest_relative" \
    --linux-image "/work/$linux_image_relative" \
    --linux-kernel "/work/$linux_symbols_relative" \
    --linux-map "/work/$linux_map_relative" \
    --linux-manifest "/work/$linux_manifest_relative" \
    --initramfs "/work/$initramfs_relative" \
    --benchmark-elf "/work/$benchmark_symbols_relative" \
    --smp "$smp" --memory "$memory" --accel "$accel" \
    --timeout-seconds "$timeout_seconds" --iterations "$iterations" \
    --repeats "$repeats" --case "$benchmark_case" --warmup "$warmup" \
    --table-bits "$table_bits" --container-image "$image"
if [ "$mode" = profile ]; then
    set -- "$@" --profile-plugin "/work/$plugin_relative" \
        --profile-start-pc "$profile_start_pc" --profile-stop-pc "$profile_stop_pc"
elif [ "$mode" = trace ]; then
    set -- "$@" --trace-plugin "/work/$plugin_relative" \
        --trace-max-instructions "$max_instructions" \
        --profile-start-pc "$profile_start_pc" --profile-stop-pc "$profile_stop_pc"
fi
"$@"

run_system() {
    system=$1
    serial_relative=$output_relative/$system.serial.log
    case "$mode" in
        profile) auxiliary_relative=$output_relative/$system.tcg-profile.txt ;;
        trace) auxiliary_relative=$output_relative/$system.instruction-trace.txt ;;
        timing) auxiliary_relative=$output_relative/$system.unused ;;
    esac
    case "$system" in
        mygo) kernel_relative=$mygo_kernel_relative ;;
        linux) kernel_relative=$linux_image_relative ;;
        *) return 2 ;;
    esac

    echo "[syscall-bench] 运行 $system：mode=$mode case=$benchmark_case iterations=$iterations repeats=$repeats"
    docker run --rm -v "$root":/work -w /work "$image" bash -c '
        set -eu
        system=$1
        mode=$2
        kernel=$3
        initramfs=$4
        serial=$5
        auxiliary=$6
        plugin=$7
        smp=$8
        memory=$9
        shift 9
        accel=$1
        timeout_seconds=$2
        iterations=$3
        repeats=$4
        benchmark_case=$5
        warmup=$6
        table_bits=$7
        profile_start_pc=$8
        profile_stop_pc=$9
        shift 9
        max_instructions=$1

        set -- qemu-system-riscv64 \
            -machine virt -global virtio-mmio.force-legacy=false \
            -accel "$accel" -bios default -kernel "$kernel" -initrd "$initramfs" \
            -m "$memory" -smp "$smp" -nographic -no-reboot -rtc base=utc
        if [ "$system" = linux ]; then
            set -- "$@" \
                -append "console=ttyS0 panic=-1 rdinit=/sbin/init syscall_bench_iterations=$iterations syscall_bench_repeats=$repeats syscall_bench_case=$benchmark_case syscall_bench_warmup=$warmup"
        else
            set -- "$@" \
                -append "syscall_bench_iterations=$iterations syscall_bench_repeats=$repeats syscall_bench_case=$benchmark_case syscall_bench_warmup=$warmup"
        fi
        if [ "$mode" = profile ]; then
            set -- "$@" -plugin "file=$plugin,output=$auxiliary,table_bits=$table_bits,start_pc=$profile_start_pc,stop_pc=$profile_stop_pc"
        elif [ "$mode" = trace ]; then
            set -- "$@" -plugin "file=$plugin,output=$auxiliary,max_instructions=$max_instructions,start_pc=$profile_start_pc,stop_pc=$profile_stop_pc"
        fi
        timeout -k 10 "$timeout_seconds" "$@" >"$serial" 2>&1
    ' run-qemu "$system" "$mode" "/work/$kernel_relative" \
        "/work/$initramfs_relative" "/work/$serial_relative" "/work/$auxiliary_relative" \
        "/work/$plugin_relative" "$smp" "$memory" "$accel" "$timeout_seconds" \
        "$iterations" "$repeats" "$benchmark_case" "$warmup" "$table_bits" \
        "$profile_start_pc" "$profile_stop_pc" "$max_instructions"

    if [ "$mode" = profile ]; then
        docker run --rm -v "$root":/work -w /work "$image" \
            scripts/profile-tcg-validate.sh "/work/$auxiliary_relative" riscv64 1 \
            "$profile_start_pc" "$profile_stop_pc"
    fi
    case "$system" in
        mygo)
            serial_option=--mygo-serial
            profile_option=--mygo-profile
            trace_option=--mygo-trace
            ;;
        linux)
            serial_option=--linux-serial
            profile_option=--linux-profile
            trace_option=--linux-trace
            ;;
    esac
    set -- docker run --rm -v "$root":/work -w /work "$image" \
        python3 scripts/syscall-bench-report.py --mode "$mode" --repo-root /work \
        --metadata "/work/$metadata_relative" --validate-only --record-system "$system" \
        "$serial_option" "/work/$serial_relative"
    if [ "$mode" = profile ]; then
        set -- "$@" "$profile_option" "/work/$auxiliary_relative"
    elif [ "$mode" = trace ]; then
        set -- "$@" "$trace_option" "/work/$auxiliary_relative"
    fi
    "$@"
    if [ "$mode" = timing ]; then
        sed -n '/^SYSCALL_/p' "$output/$system.serial.log"
    else
        sed -n \
            -e '/^SYSCALL_BENCH /p' \
            -e '/^SYSCALL_BENCH_DONE /p' \
            -e '/^SYSCALL_GUEST_DONE /p' \
            "$output/$system.serial.log"
    fi
}

case "$systems" in
    mygo) run_system mygo ;;
    linux) run_system linux ;;
    both)
        run_system mygo
        run_system linux
        ;;
esac

if [ "$systems" = both ]; then
    set -- docker run --rm -v "$root":/work -w /work "$image" \
        python3 scripts/syscall-bench-report.py --mode "$mode" --repo-root /work \
        --metadata "/work/$metadata_relative" \
        --mygo-serial "/work/$output_relative/mygo.serial.log" \
        --linux-serial "/work/$output_relative/linux.serial.log"
    if [ "$mode" = profile ]; then
        set -- "$@" \
            --mygo-profile "/work/$output_relative/mygo.tcg-profile.txt" \
            --linux-profile "/work/$output_relative/linux.tcg-profile.txt"
    elif [ "$mode" = trace ]; then
        set -- "$@" \
            --mygo-trace "/work/$output_relative/mygo.instruction-trace.txt" \
            --linux-trace "/work/$output_relative/linux.instruction-trace.txt"
    fi
    "$@"
    if [ "$mode" = trace ]; then
        docker run --rm -v "$root":/work -w /work "$image" \
            python3 scripts/syscall-instruction-compare.py \
            --mygo-trace "/work/$output_relative/mygo.instruction-trace.txt" \
            --linux-trace "/work/$output_relative/linux.instruction-trace.txt" \
            --benchmark-elf "/work/$benchmark_symbols_relative" \
            --mygo-kernel "/work/$mygo_kernel_relative" \
            --linux-vmlinux "/work/$linux_symbols_relative" \
            --mygo-output "/work/$output_relative/mygo.instruction-sequence.tsv" \
            --linux-output "/work/$output_relative/linux.instruction-sequence.tsv"
    fi
fi

echo "[syscall-bench] 输出目录：$output"
