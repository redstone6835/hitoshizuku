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

if grep -Eq '^module_count=[1-9][0-9]*$' "$manifest"; then
    ELM_BUILD_BOUND_MANIFEST=$manifest
    export ELM_BUILD_BOUND_MANIFEST
fi

exec "$@"
