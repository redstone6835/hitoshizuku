#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
python=${RISCV_WEIGHT_ML_PYTHON:-python3}
venv=${RISCV_WEIGHT_ML_VENV:-$root/build/venv-riscv-instruction-ml}
case "$venv" in /*) ;; *) venv=$root/$venv ;; esac
venv=$(realpath -m -- "$venv")
requirements=$root/scripts/requirements-riscv-instruction-ml.txt
stamp=$venv/.mygo-requirements.sha256

case "$venv" in
    "$root"/*) ;;
    *)
        echo "RISCV_WEIGHT_ML_VENV must be inside the repository" >&2
        exit 2
        ;;
esac

requirements_hash=$(sha256sum "$requirements" | awk '{print $1}')
installed_hash=
if [ -r "$stamp" ]; then
    installed_hash=$(sed -n '1p' "$stamp")
fi

if [ ! -x "$venv/bin/python" ] || [ "$installed_hash" != "$requirements_hash" ]; then
    mkdir -p "$(dirname "$venv")"
    rm -rf -- "$venv"
    "$python" -m venv "$venv"
    "$venv/bin/python" -m pip install --disable-pip-version-check \
        --requirement "$requirements" >&2
    printf '%s\n' "$requirements_hash" >"$stamp"
fi

"$venv/bin/python" -c \
    'import numpy, sklearn; print(sklearn.__version__, numpy.__version__)' \
    >/dev/null
printf '%s\n' "$venv/bin/python"
