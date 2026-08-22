#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
wrapper=$root/scripts/kcsan-rustc-wrapper.sh
rustc=${RUSTC:-rustc}

if [ -n "${LLVM_NM:-}" ]; then
    llvm_nm=$LLVM_NM
elif command -v llvm-nm >/dev/null 2>&1; then
    llvm_nm=$(command -v llvm-nm)
else
    echo "llvm-nm is required" >&2
    exit 1
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/hitoshizuku-kcsan-codegen.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/probe.rs" <<'EOF'
#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kcsan_codegen_probe(
    plain: *mut u32,
    volatile: *mut u32,
    atomic: *const AtomicU32,
    source: *const u8,
    destination: *mut u8,
    length: usize,
) -> u32 {
    unsafe {
        *plain += 1;
        let value = core::ptr::read_volatile(volatile);
        core::ptr::write_volatile(volatile, value.wrapping_add(1));
        (*atomic).fetch_add(1, Ordering::Relaxed);
        core::ptr::copy_nonoverlapping(source, destination, length);
        *plain
    }
}
EOF

fail() {
    echo "kcsan codegen test: $*" >&2
    exit 1
}

check_instrumented_object() {
    target=$1
    object=$tmp/probe-$target.o
    symbols=$tmp/probe-$target.symbols

    "$wrapper" "$rustc" \
        --crate-name kcsan_probe \
        --crate-type lib \
        --edition 2024 \
        --target "$target" \
        -Copt-level=1 \
        --emit "obj=$object" \
        "$tmp/probe.rs"
    "$llvm_nm" --undefined-only "$object" >"$symbols"

    grep -Eq '__tsan_read_write(1|2|4|8|16)' "$symbols" ||
        fail "$target did not emit a compound read/write hook"
    grep -Eq '__tsan_volatile_read(1|2|4|8|16)' "$symbols" ||
        fail "$target did not emit a volatile read hook"
    grep -Eq '__tsan_volatile_write(1|2|4|8|16)' "$symbols" ||
        fail "$target did not emit a volatile write hook"
    if grep -Eq '__tsan_atomic|__tsan_mem(cpy|move|set)' "$symbols"; then
        fail "$target emitted a disabled atomic or memory-intrinsic hook"
    fi
}

check_runtime_excluded() {
    target=$1
    object=$tmp/kcsan-$target.o
    symbols=$tmp/kcsan-$target.symbols

    "$wrapper" "$rustc" \
        --crate-name kcsan \
        --crate-type lib \
        --edition 2024 \
        --target="$target" \
        -Copt-level=1 \
        --emit "obj=$object" \
        "$tmp/probe.rs"
    "$llvm_nm" --undefined-only "$object" >"$symbols"

    if grep -q '__tsan_' "$symbols"; then
        fail "$target instrumented the kcsan runtime crate"
    fi
}

for target in loongarch64-unknown-none riscv64gc-unknown-none-elf; do
    check_instrumented_object "$target"
    check_runtime_excluded "$target"
done

echo "kcsan codegen test: passed"
