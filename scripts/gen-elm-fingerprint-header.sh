#!/bin/sh
set -eu

target=${1:?target identifier is required}
output=${2:?output path is required}

# 由 elm crate 根据真实 ABI 类型布局生成，禁止在脚本中维护第二份 fingerprint 规则。
cargo run --quiet --manifest-path tools/elm-tools/Cargo.toml \
    --target x86_64-unknown-linux-gnu -- internal-fingerprint-header "$target" "$output"
