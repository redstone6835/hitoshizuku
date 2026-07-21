#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/bin"

cat >"$tmp/bin/dd" <<'EOF'
#!/bin/sh
for arg in "$@"; do
    case "$arg" in
        of=*) output=${arg#of=} ;;
    esac
done
[ -n "${output:-}" ] || exit 2
input=$(cat)
[ -z "${PROFILE_DD_LOG:-}" ] || printf '%s\n' "$input" >>"$PROFILE_DD_LOG"
printf '%s\n' "$input" >"$output"
EOF
chmod +x "$tmp/bin/dd"

control="$tmp/profile_control"
stats="$tmp/profile_stats"
trace="$tmp/profile_trace"
: >"$control"
cat >"$stats" <<'EOF'
state=frozen enabled=0 session=9 generation=2 active_writers=0
EOF
cat >"$trace" <<'EOF'
state=frozen enabled=0 session=9 generation=2 active_writers=0 trace=1 counter_hz=1000000 slots_per_cpu=1024 record_bytes=80 format_version=2
cpu=0 first_sequence=0 next_sequence=0 retained=0 overwritten=0
EOF

output="$tmp/capture.log"
set +e
PATH="$tmp/bin:$PATH" \
PROFILE_CONTROL="$control" \
PROFILE_STATS="$stats" \
PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_ARCH=riscv64 \
PROFILE_CPU_ONLINE=0-1 \
PROFILE_KERNEL_RELEASE=mygo \
PROFILE_KERNEL_IMAGE_ID=kernel-sha256 \
PROFILE_ROOTFS_IMAGE_ID=rootfs-sha256 \
PROFILE_CMDLINE='console=ttyS0' \
PROFILE_DD_LOG="$tmp/dd.log" \
PROFILE_PRESET=io \
    "$root/scripts/profile-capture.sh" run smoke /bin/sh -c 'exit 7' >"$output"
status=$?
set -e

[ "$status" -eq 7 ]
grep -q '^@@PROFILE_META_BEGIN phase=before case=smoke$' "$output"
grep -q '^@@PROFILE_META_BEGIN phase=after case=smoke$' "$output"
grep -q '^workload=/bin/sh -c exit 7$' "$output"
grep -q '^workload_exit_status=7$' "$output"
grep -Eq '^@@PROFILE_WORKLOAD case=smoke pid=[0-9]+$' "$output"
grep -q '^kernel_image_id=kernel-sha256$' "$output"
grep -q '^rootfs_image_id=rootfs-sha256$' "$output"
grep -q '^@@PROFILE_TRACE_END phase=after case=smoke$' "$output"
grep -q '^events=0x1e3ff4000$' "$tmp/dd.log"

printf 'token\n' | PATH="$tmp/bin:$PATH" \
    PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    PROFILE_SAMPLES="$tmp/missing-samples" \
    PROFILE_TRACE_FILE="$trace" \
    PROFILE_CMDLINE='' \
    "$root/scripts/profile-capture.sh" run stdin /bin/sh -c \
        'read value && [ "$value" = token ]' >/dev/null

if PATH="$tmp/bin:$PATH" \
    PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    PROFILE_EVENT_MASK=0x1 \
    PROFILE_PRESET=io \
    "$root/scripts/profile-capture.sh" start conflict >/dev/null 2>&1; then
    echo "profile-capture fixture: conflicting event selectors were accepted" >&2
    exit 1
fi

echo "profile-capture fixture: ok"
