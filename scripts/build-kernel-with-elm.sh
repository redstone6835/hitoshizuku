#!/bin/sh
set -eu

if [ "$#" -lt 3 ]; then
    echo "usage: $0 <modules.manifest> <integrated.archives> <cargo> [args...]" >&2
    exit 2
fi

manifest=$1
archives=$2
shift 2

unset ELM_BUILD_BOUND_MANIFEST
unset ELM_INTEGRATED_ARCHIVES

if [ -s "$archives" ]; then
    ELM_INTEGRATED_ARCHIVES=$(paste -sd: "$archives")
    export ELM_INTEGRATED_ARCHIVES
fi

if [ "${ELM_BIND_MODULES:-1}" != "0" ] && grep -Eq '^module_count=[1-9][0-9]*$' "$manifest"; then
    ELM_BUILD_BOUND_MANIFEST=$manifest
    export ELM_BUILD_BOUND_MANIFEST
fi

if [ -n "${KERNEL_LINK_MAP:-}" ]; then
    for value in "$KERNEL_LINK_MAP" "${KERNEL_LINK_OUTPUT:-}" "${KERNEL_LINK_SOURCE:-}"; do
        case "$value" in
            /*) ;;
            *) echo "kernel link map paths must be absolute" >&2; exit 2 ;;
        esac
    done
    root_output=${KERNEL_LINK_ROOT_OUTPUT:-}
    if [ -n "$root_output" ]; then
        case "$root_output" in
            /*) ;;
            *) echo "KERNEL_LINK_ROOT_OUTPUT must be absolute" >&2; exit 2 ;;
        esac
    fi
    case "${KERNEL_LINK_TARGET:-}" in
        ''|*[!A-Za-z0-9_.-]*)
            echo "KERNEL_LINK_TARGET has invalid syntax" >&2
            exit 2
            ;;
    esac
    [ "$#" -ge 2 ] && [ "$2" = build ] || {
        echo "KERNEL_LINK_MAP requires a cargo build command" >&2
        exit 2
    }
    cargo=$1
    shift 2
    map_dir=$(dirname "$KERNEL_LINK_MAP")
    output_dir=$(dirname "$KERNEL_LINK_OUTPUT")
    source_dir=$(dirname "$KERNEL_LINK_SOURCE")
    manifest="$KERNEL_LINK_MAP.manifest"
    [ "$KERNEL_LINK_MAP" != "$KERNEL_LINK_OUTPUT" ] && \
        [ "$KERNEL_LINK_MAP" != "$KERNEL_LINK_SOURCE" ] && \
        [ "$KERNEL_LINK_OUTPUT" != "$KERNEL_LINK_SOURCE" ] && \
        [ "$manifest" != "$KERNEL_LINK_OUTPUT" ] && \
        [ "$manifest" != "$KERNEL_LINK_SOURCE" ] || {
        echo "kernel link map, manifest, source, and output paths must be distinct" >&2
        exit 2
    }
    publish_root=0
    if [ -n "$root_output" ] && [ "$root_output" != "$KERNEL_LINK_OUTPUT" ]; then
        [ "$root_output" != "$KERNEL_LINK_MAP" ] && \
            [ "$root_output" != "$manifest" ] && \
            [ "$root_output" != "$KERNEL_LINK_SOURCE" ] || {
            echo "kernel root publish output conflicts with another link artifact" >&2
            exit 2
        }
        root_dir=$(dirname "$root_output")
        publish_root=1
        mkdir -p "$root_dir"
    fi
    mkdir -p "$map_dir" "$output_dir" "$source_dir"

    exec 9>"$KERNEL_LINK_SOURCE.lock"
    flock -n 9 || {
        echo "another kernel link owns cargo output $KERNEL_LINK_SOURCE" >&2
        exit 1
    }
    exec 8>"$KERNEL_LINK_OUTPUT.lock"
    flock -n 8 || {
        echo "another kernel link is publishing $KERNEL_LINK_OUTPUT" >&2
        exit 1
    }
    exec 7>"$KERNEL_LINK_MAP.lock"
    flock -n 7 || {
        echo "another kernel link is publishing $KERNEL_LINK_MAP" >&2
        exit 1
    }
    if [ "$publish_root" -eq 1 ]; then
        exec 6>"$root_output.lock"
        flock -n 6 || {
            echo "another kernel link is publishing $root_output" >&2
            exit 1
        }
    fi

    map_tmp=
    output_tmp=
    root_tmp=
    manifest_tmp=
    cleanup_link_temps() {
        [ -z "$map_tmp" ] || rm -f "$map_tmp"
        [ -z "$output_tmp" ] || rm -f "$output_tmp"
        [ -z "$root_tmp" ] || rm -f "$root_tmp"
        [ -z "$manifest_tmp" ] || rm -f "$manifest_tmp"
    }
    trap cleanup_link_temps 0 1 2 15
    map_tmp=$(mktemp "$KERNEL_LINK_MAP.tmp.XXXXXX")
    output_tmp=$(mktemp "$KERNEL_LINK_OUTPUT.tmp.XXXXXX")
    if [ "$publish_root" -eq 1 ]; then
        root_tmp=$(mktemp "$root_output.tmp.XXXXXX")
    fi
    manifest_tmp=$(mktemp "$manifest.tmp.XXXXXX")
    "$cargo" rustc "$@" -- -C "link-arg=-Map=$map_tmp"
    test -s "$map_tmp" && test -s "$KERNEL_LINK_SOURCE"
    cp -p "$KERNEL_LINK_SOURCE" "$output_tmp"
    test -s "$output_tmp"
    if [ "$publish_root" -eq 1 ]; then
        cp -p "$KERNEL_LINK_SOURCE" "$root_tmp"
        test -s "$root_tmp"
        cmp -s "$output_tmp" "$root_tmp"
    fi
    chmod 0644 "$map_tmp"
    kernel_digest=$(sha256sum "$output_tmp")
    kernel_sha256=${kernel_digest%% *}
    map_digest=$(sha256sum "$map_tmp")
    map_sha256=${map_digest%% *}
    case "$kernel_sha256:$map_sha256" in
        *[!0-9a-f:]*|:*) echo "kernel link artifact SHA-256 failed" >&2; exit 1 ;;
    esac
    [ "${#kernel_sha256}" -eq 64 ] && [ "${#map_sha256}" -eq 64 ] || {
        echo "kernel link artifact SHA-256 has invalid length" >&2
        exit 1
    }
    {
        printf 'schema=mygo.kernel-map-manifest.v1\n'
        printf 'target=%s\n' "$KERNEL_LINK_TARGET"
        printf 'kernel_sha256=%s\n' "$kernel_sha256"
        printf 'symbol_map_sha256=%s\n' "$map_sha256"
    } >"$manifest_tmp"
    chmod 0644 "$manifest_tmp"
    mv -T "$output_tmp" "$KERNEL_LINK_OUTPUT"
    output_tmp=
    if [ "$publish_root" -eq 1 ]; then
        mv -T "$root_tmp" "$root_output"
        root_tmp=
    fi
    mv -T "$map_tmp" "$KERNEL_LINK_MAP"
    map_tmp=
    mv -T "$manifest_tmp" "$manifest"
    manifest_tmp=
    trap - 0 1 2 15
    exit 0
fi

exec "$@"
