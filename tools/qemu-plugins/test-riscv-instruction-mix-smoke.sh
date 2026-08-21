#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
plugin=${1:-"$root/build/qemu-plugins/riscv_instruction_mix.so"}
kernel=${2:-"$root/kernel-rv"}
image=${QEMU_PLUGIN_CONTAINER_IMAGE:-}
[ -n "$image" ] || {
    echo "QEMU_PLUGIN_CONTAINER_IMAGE must name a build image" >&2
    exit 2
}
temporary=$(mktemp -d)
container="mygo-riscv-mix-smoke-$$"

cleanup() {
    docker rm -f "$container" >/dev/null 2>&1 || true
    rm -rf "$temporary"
}
trap cleanup EXIT INT TERM

test -r "$plugin" || { echo "missing plugin: $plugin" >&2; exit 1; }
test -r "$kernel" || { echo "missing kernel: $kernel" >&2; exit 1; }
case "$plugin:$kernel" in
    "$root"/*:"$root"/*) ;;
    *) echo "plugin and kernel must be inside the repository" >&2; exit 1 ;;
esac
plugin_relative=${plugin#"$root"/}
kernel_relative=${kernel#"$root"/}

printf '0' >"$temporary/control"
chmod 0777 "$temporary" "$temporary/control"
docker run -d --name "$container" \
    -v "$root":/work:ro -v "$temporary":/out "$image" \
    qemu-system-riscv64 \
    -machine virt -accel tcg,thread=multi -m 512M -smp 1 \
    -display none -monitor none -serial none -no-reboot \
    -kernel "/work/$kernel_relative" \
    -plugin "/work/$plugin_relative,output=/out/mix.jsonl,catalog=/out/catalog.jsonl,control=/out/control,epoch-ms=1000" \
    >/dev/null

attempt=0
while [ "$attempt" -lt 50 ]; do
    [ -s "$temporary/catalog.jsonl" ] && [ -s "$temporary/mix.jsonl" ] && break
    running=$(docker inspect --format '{{.State.Running}}' "$container" 2>/dev/null || true)
    [ "$running" = true ] || break
    attempt=$((attempt + 1))
    sleep 0.1
done
docker stop -t 10 "$container" >/dev/null 2>&1 || true
timeout 20 docker wait "$container" >/dev/null 2>&1 || true

python3 - "$temporary/catalog.jsonl" "$temporary/mix.jsonl" <<'PY'
import json
import pathlib
import sys


def first_and_last(path: pathlib.Path) -> tuple[dict[str, object], dict[str, object]]:
    with path.open("rb") as stream:
        first = stream.readline()
        if not first:
            raise SystemExit(f"empty smoke output: {path}")
        stream.seek(0, 2)
        end = stream.tell()
        position = max(0, end - 65536)
        stream.seek(position)
        tail = stream.read()
    lines = tail.splitlines()
    if position and lines:
        lines = lines[1:]
    if not lines:
        raise SystemExit(f"missing smoke tail: {path}")
    return json.loads(first), json.loads(lines[-1])


catalog_header, catalog_quality = first_and_last(pathlib.Path(sys.argv[1]))
mix_header, mix_quality = first_and_last(pathlib.Path(sys.argv[2]))
expected_policy = {
    "flush_records": 4096,
    "buffer_bytes": 4 * 1024 * 1024,
    "flush_policy": "bounded-batch-v1",
    "tail_failure": "missing-final-quality-invalid",
}
for owner, record in (("catalog header", catalog_header),
                      ("catalog quality", catalog_quality),
                      ("mix catalog quality", mix_quality.get("catalog", {}))):
    for key, value in expected_policy.items():
        if record.get(key) != value:
            raise SystemExit(f"{owner} has invalid {key}: {record.get(key)!r}")
if catalog_quality.get("type") != "quality" or mix_quality.get("type") != "quality":
    raise SystemExit("QEMU shutdown did not emit final quality records")
records = catalog_quality.get("records")
if not isinstance(records, int) or records <= 0:
    raise SystemExit("catalog smoke contains no translation records")
if records != catalog_quality.get("translated_blocks"):
    raise SystemExit("catalog translation totals do not close")
if records != mix_quality.get("translated_blocks"):
    raise SystemExit("mix/catalog translation totals do not close")
for key in ("write_errors", "dropped_blocks", "tracking_drops"):
    if catalog_quality.get(key) != 0:
        raise SystemExit(f"catalog smoke reports {key}={catalog_quality.get(key)!r}")
if catalog_quality.get("pc_seen_entries", 0) > catalog_quality.get("pc_seen_slots", 0):
    raise SystemExit("PC seen table exceeds its capacity")
if catalog_quality.get("fingerprint_seen_entries", 0) > catalog_quality.get(
    "fingerprint_seen_slots", 0
):
    raise SystemExit("fingerprint seen table exceeds its capacity")
expected_flushes = records // expected_policy["flush_records"] + 2
if catalog_quality.get("flushes") != expected_flushes:
    raise SystemExit(
        f"catalog flush count is not bounded-batch exact: "
        f"{catalog_quality.get('flushes')!r} != {expected_flushes}"
    )
if mix_header.get("type") != "header" or catalog_header.get("type") != "header":
    raise SystemExit("smoke headers are missing")
print(
    f"riscv instruction mix smoke: records={records} "
    f"flushes={catalog_quality['flushes']} tracking_drops=0"
)
PY
