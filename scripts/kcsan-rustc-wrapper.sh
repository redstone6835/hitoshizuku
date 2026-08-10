#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: $0 <rustc> [rustc arguments...]" >&2
    exit 2
fi

rustc=$1
shift

target=
crate_name=
next_value=
for argument do
    if [ -n "$next_value" ]; then
        case "$next_value" in
            target) target=$argument ;;
            crate_name) crate_name=$argument ;;
        esac
        next_value=
        continue
    fi

    case "$argument" in
        --target)
            next_value=target
            ;;
        --target=*)
            target=${argument#--target=}
            ;;
        --crate-name)
            next_value=crate_name
            ;;
        --crate-name=*)
            crate_name=${argument#--crate-name=}
            ;;
    esac
done

case "$target" in
    loongarch64-unknown-none|riscv64gc-unknown-none-elf)
        if [ "$crate_name" != kcsan ]; then
            exec "$rustc" "$@" \
                -Cpasses=forceattrs,tsan \
                -Cllvm-args=-force-attribute=sanitize_thread \
                -Cllvm-args=-tsan-instrument-func-entry-exit=0 \
                -Cllvm-args=-tsan-instrument-atomics=0 \
                -Cllvm-args=-tsan-instrument-memintrinsics=0 \
                -Cllvm-args=-tsan-compound-read-before-write=1 \
                -Cllvm-args=-tsan-distinguish-volatile=1
        fi
        ;;
esac

exec "$rustc" "$@"
