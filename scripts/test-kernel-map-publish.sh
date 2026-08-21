#!/bin/sh
set -eu

# The fixture doubles as fake cargo/cp/mv so no helper executable needs to be
# generated in the repository.
case ${0##*/} in
    cp)
        if [ "${PUBLISH_FIXTURE_CP_PARTIAL_FAIL:-0}" = 1 ] && \
            [ ! -e "$PUBLISH_FIXTURE_CP_STATE" ]; then
            destination=
            for destination in "$@"; do :; done
            printf 'partial-kernel\n' >"$destination"
            : >"$PUBLISH_FIXTURE_CP_STATE"
            exit 74
        fi
        exec "$PUBLISH_FIXTURE_REAL_CP" "$@"
        ;;
    mv)
        count=0
        if [ -s "$PUBLISH_FIXTURE_MV_STATE" ]; then
            count=$(sed -n '1p' "$PUBLISH_FIXTURE_MV_STATE")
        fi
        count=$((count + 1))
        printf '%s\n' "$count" >"$PUBLISH_FIXTURE_MV_STATE"
        if [ "$count" -eq "${PUBLISH_FIXTURE_MV_FAIL_AT:-0}" ]; then
            exit 75
        fi
        exec "$PUBLISH_FIXTURE_REAL_MV" "$@"
        ;;
esac

if [ "${1:-}" = rustc ]; then
    link_map=
    for argument in "$@"; do
        case "$argument" in
            link-arg=-Map=*) link_map=${argument#link-arg=-Map=} ;;
        esac
    done
    [ -n "$link_map" ] || {
        echo "fake cargo: missing linker map argument" >&2
        exit 70
    }
    if [ -n "${PUBLISH_FIXTURE_ENTERED:-}" ]; then
        : >"$PUBLISH_FIXTURE_ENTERED"
    fi
    if [ -n "${PUBLISH_FIXTURE_RELEASE:-}" ]; then
        while [ ! -e "$PUBLISH_FIXTURE_RELEASE" ]; do
            sleep 0.01
        done
    fi
    printf '%s\n' "${PUBLISH_FIXTURE_KERNEL:-fixture-kernel}" >"$KERNEL_LINK_SOURCE"
    printf '%s\n' "${PUBLISH_FIXTURE_MAP:-fixture-map}" >"$link_map"
    [ "${PUBLISH_FIXTURE_CARGO_FAIL:-0}" -eq 0 ] || exit 71
    exit 0
fi

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
self=$repo/scripts/test-kernel-map-publish.sh
builder=$repo/scripts/build-kernel-with-elm.sh
root=$(mktemp -d)
first_pid=

cleanup() {
    if [ -n "$first_pid" ]; then
        kill -TERM "$first_pid" 2>/dev/null || true
        wait "$first_pid" 2>/dev/null || true
    fi
    rm -rf "$root"
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "kernel map publish fixture: $*" >&2
    exit 1
}

assert_contents() {
    expected=$1
    path=$2
    [ -f "$path" ] || fail "missing $path"
    actual=$(sed -n '1p' "$path")
    [ "$actual" = "$expected" ] || {
        fail "$path contains '$actual', expected '$expected'"
    }
}

assert_no_temps() {
    directory=$1
    temporary=$(find "$directory" -type f -name '*.tmp.*' -print -quit)
    [ -z "$temporary" ] || fail "temporary artifact survived: $temporary"
}

manifest_matches() {
    kernel=$1
    symbol_map=$2
    manifest=$3
    kernel_digest=$(sha256sum "$kernel")
    kernel_sha256=${kernel_digest%% *}
    map_digest=$(sha256sum "$symbol_map")
    map_sha256=${map_digest%% *}
    grep -Fqx 'schema=mygo.kernel-map-manifest.v1' "$manifest" &&
        grep -Fqx 'target=fixture-target' "$manifest" &&
        grep -Fqx "kernel_sha256=$kernel_sha256" "$manifest" &&
        grep -Fqx "symbol_map_sha256=$map_sha256" "$manifest"
}

write_snapshot() {
    output=$1
    root_output=$2
    symbol_map=$3
    kernel_contents=$4
    map_contents=$5
    mkdir -p "$(dirname "$output")" "$(dirname "$root_output")" \
        "$(dirname "$symbol_map")"
    printf '%s\n' "$kernel_contents" >"$output"
    printf '%s\n' "$kernel_contents" >"$root_output"
    printf '%s\n' "$map_contents" >"$symbol_map"
    kernel_digest=$(sha256sum "$output")
    kernel_sha256=${kernel_digest%% *}
    map_digest=$(sha256sum "$symbol_map")
    map_sha256=${map_digest%% *}
    {
        printf 'schema=mygo.kernel-map-manifest.v1\n'
        printf 'target=fixture-target\n'
        printf 'kernel_sha256=%s\n' "$kernel_sha256"
        printf 'symbol_map_sha256=%s\n' "$map_sha256"
    } >"$symbol_map.manifest"
}

modules=$root/modules.manifest
archives=$root/integrated.archives
: >"$modules"
: >"$archives"

# Different map destinations must still conflict when cargo source/output are
# shared. The loser must fail before fake cargo starts.
concurrent=$root/concurrent
mkdir -p "$concurrent"
source_kernel=$concurrent/cargo/kernel
published_kernel=$concurrent/build/kernel
map_a=$concurrent/map-a
map_b=$concurrent/map-b
entered_a=$concurrent/entered-a
entered_b=$concurrent/entered-b
release_a=$concurrent/release-a
ELM_BIND_MODULES=0 \
KERNEL_LINK_MAP=$map_a \
KERNEL_LINK_OUTPUT=$published_kernel \
KERNEL_LINK_SOURCE=$source_kernel \
KERNEL_LINK_TARGET=fixture-target \
PUBLISH_FIXTURE_KERNEL=kernel-a \
PUBLISH_FIXTURE_MAP=map-a \
PUBLISH_FIXTURE_ENTERED=$entered_a \
PUBLISH_FIXTURE_RELEASE=$release_a \
    "$builder" "$modules" "$archives" "$self" build -p fixture &
first_pid=$!
attempts=0
while [ ! -e "$entered_a" ] && [ "$attempts" -lt 500 ]; do
    kill -0 "$first_pid" 2>/dev/null || fail "first publisher exited before fake cargo"
    attempts=$((attempts + 1))
    sleep 0.01
done
[ -e "$entered_a" ] || fail "first publisher did not acquire its locks"
if ELM_BIND_MODULES=0 \
    KERNEL_LINK_MAP=$map_b \
    KERNEL_LINK_OUTPUT=$published_kernel \
    KERNEL_LINK_SOURCE=$source_kernel \
    KERNEL_LINK_TARGET=fixture-target \
    PUBLISH_FIXTURE_KERNEL=kernel-b \
    PUBLISH_FIXTURE_MAP=map-b \
    PUBLISH_FIXTURE_ENTERED=$entered_b \
        "$builder" "$modules" "$archives" "$self" build -p fixture \
        >"$concurrent/second.stdout" 2>"$concurrent/second.stderr"; then
    fail "different map paths bypassed the shared cargo/output lock"
fi
[ ! -e "$entered_b" ] || fail "losing publisher reached cargo"
: >"$release_a"
wait "$first_pid"
first_pid=
assert_contents kernel-a "$published_kernel"
assert_contents map-a "$map_a"
[ ! -e "$map_b" ] || fail "losing publisher created its map"
manifest_matches "$published_kernel" "$map_a" "$map_a.manifest" || {
    fail "winning concurrent publisher produced an invalid manifest"
}

# A root kernel output is copied completely before any destination is renamed,
# and the one kernel hash in the manifest binds both published copies.
root_case=$root/root-output
source_kernel=$root_case/cargo/kernel
published_kernel=$root_case/build/kernel
root_kernel=$root_case/kernel-la
symbol_map=$root_case/kernel.map
ELM_BIND_MODULES=0 \
KERNEL_LINK_MAP=$symbol_map \
KERNEL_LINK_OUTPUT=$published_kernel \
KERNEL_LINK_SOURCE=$source_kernel \
KERNEL_LINK_ROOT_OUTPUT=$root_kernel \
KERNEL_LINK_TARGET=fixture-target \
PUBLISH_FIXTURE_KERNEL=root-kernel \
PUBLISH_FIXTURE_MAP=root-map \
    "$builder" "$modules" "$archives" "$self" build -p fixture
assert_contents root-kernel "$published_kernel"
assert_contents root-kernel "$root_kernel"
assert_contents root-map "$symbol_map"
manifest_matches "$root_kernel" "$symbol_map" "$symbol_map.manifest" || {
    fail "manifest does not bind the root kernel output"
}
for lock in "$source_kernel.lock" "$published_kernel.lock" \
    "$root_kernel.lock" "$symbol_map.lock"; do
    [ -f "$lock" ] || fail "missing resource lock $lock"
done

# A copy that writes only a partial temporary kernel must leave every published
# artifact untouched and must be cleaned up.
fake_bin=$root/fake-bin
mkdir -p "$fake_bin"
ln -s "$self" "$fake_bin/cp"
ln -s "$self" "$fake_bin/mv"
real_cp=$(command -v cp)
real_mv=$(command -v mv)
copy_case=$root/copy-failure
source_kernel=$copy_case/cargo/kernel
published_kernel=$copy_case/build/kernel
root_kernel=$copy_case/kernel-la
symbol_map=$copy_case/kernel.map
write_snapshot "$published_kernel" "$root_kernel" "$symbol_map" old-kernel old-map
if PATH="$fake_bin:$PATH" \
    PUBLISH_FIXTURE_REAL_CP=$real_cp \
    PUBLISH_FIXTURE_CP_PARTIAL_FAIL=1 \
    PUBLISH_FIXTURE_CP_STATE=$copy_case/cp-state \
    ELM_BIND_MODULES=0 \
    KERNEL_LINK_MAP=$symbol_map \
    KERNEL_LINK_OUTPUT=$published_kernel \
    KERNEL_LINK_SOURCE=$source_kernel \
    KERNEL_LINK_ROOT_OUTPUT=$root_kernel \
    KERNEL_LINK_TARGET=fixture-target \
    PUBLISH_FIXTURE_KERNEL=new-kernel \
    PUBLISH_FIXTURE_MAP=new-map \
        "$builder" "$modules" "$archives" "$self" build -p fixture \
        >"$copy_case/stdout" 2>"$copy_case/stderr"; then
    fail "partial copy failure was accepted"
fi
assert_contents old-kernel "$published_kernel"
assert_contents old-kernel "$root_kernel"
assert_contents old-map "$symbol_map"
manifest_matches "$root_kernel" "$symbol_map" "$symbol_map.manifest" || {
    fail "partial copy damaged the old committed snapshot"
}
assert_no_temps "$copy_case"

# Failure after both full kernel renames but before the map rename may replace
# whole kernels, but the old manifest must remain and make the tuple fail closed.
move_case=$root/move-failure
source_kernel=$move_case/cargo/kernel
published_kernel=$move_case/build/kernel
root_kernel=$move_case/kernel-la
symbol_map=$move_case/kernel.map
write_snapshot "$published_kernel" "$root_kernel" "$symbol_map" old-kernel old-map
if PATH="$fake_bin:$PATH" \
    PUBLISH_FIXTURE_REAL_CP=$real_cp \
    PUBLISH_FIXTURE_REAL_MV=$real_mv \
    PUBLISH_FIXTURE_MV_STATE=$move_case/mv-state \
    PUBLISH_FIXTURE_MV_FAIL_AT=3 \
    ELM_BIND_MODULES=0 \
    KERNEL_LINK_MAP=$symbol_map \
    KERNEL_LINK_OUTPUT=$published_kernel \
    KERNEL_LINK_SOURCE=$source_kernel \
    KERNEL_LINK_ROOT_OUTPUT=$root_kernel \
    KERNEL_LINK_TARGET=fixture-target \
    PUBLISH_FIXTURE_KERNEL=new-kernel \
    PUBLISH_FIXTURE_MAP=new-map \
        "$builder" "$modules" "$archives" "$self" build -p fixture \
        >"$move_case/stdout" 2>"$move_case/stderr"; then
    fail "injected map rename failure was accepted"
fi
assert_contents new-kernel "$published_kernel"
assert_contents new-kernel "$root_kernel"
assert_contents old-map "$symbol_map"
if manifest_matches "$root_kernel" "$symbol_map" "$symbol_map.manifest"; then
    fail "partial publication appeared committed"
fi
assert_no_temps "$move_case"

echo "kernel map publication fixtures: ok"
