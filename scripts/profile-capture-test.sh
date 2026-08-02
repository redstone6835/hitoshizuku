#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/bin"

cat >"$tmp/bin/dd" <<'EOF'
#!/bin/sh
echo invoked >>"$PROFILE_DD_LOG"
exit 99
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
PROFILE_HEALTH="$tmp/missing-health" \
PROFILE_ARCH=riscv64 \
PROFILE_CPU_ONLINE=0-1 \
PROFILE_KERNEL_RELEASE=mygo \
PROFILE_KERNEL_IMAGE_ID=kernel-sha256 \
PROFILE_ROOTFS_IMAGE_ID=rootfs-sha256 \
PROFILE_CMDLINE='console=ttyS0' \
PROFILE_DD_LOG="$tmp/dd.log" \
PROFILE_PRESET=io \
PROFILE_TIMING_SHIFT=4 \
PROFILE_TIMING_SAMPLER=hashed-bernoulli-v1 \
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
grep -q '^control=timing_shift=4$' "$output"
grep -q '^timing_sampler=hashed-bernoulli-v1$' "$output"
grep -q '^@@PROFILE_TRACE_END phase=after case=smoke$' "$output"
[ ! -e "$tmp/dd.log" ]
[ "$(cat "$control")" = freeze ]

printf 'token\n' | PATH="$tmp/bin:$PATH" \
    PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_HEALTH="$tmp/missing-health" \
    PROFILE_CMDLINE='' \
    "$root/scripts/profile-capture.sh" run stdin /bin/sh -c \
        'read value && [ "$value" = token ]' >"$output"
grep -q '^control=timing_shift=8$' "$output"

if PATH="$tmp/bin:$PATH" \
    PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    PROFILE_EVENT_MASK=0x1 \
    PROFILE_PRESET=io \
    "$root/scripts/profile-capture.sh" start conflict >/dev/null 2>&1; then
    echo "profile-capture fixture: conflicting event selectors were accepted" >&2
    exit 1
fi

grep -q 'io|syscall|filesystem|memory|scheduler|block|network|build|all|full)' \
    "$root/scripts/profile-capture.sh"
grep -q 'write_control "preset=$preset"' "$root/scripts/profile-capture.sh"
grep -q 'write_control "events=$PROFILE_EVENT_MASK"' "$root/scripts/profile-capture.sh"
grep -q 'write_control "events_high=$PROFILE_EVENT_MASK_HIGH"' "$root/scripts/profile-capture.sh"
grep -q 'event_mask=${PROFILE_EVENT_MASK:-0xfef000000}' "$root/scripts/buildstorm-profile-host.sh"
grep -q 'event_mask_high=${PROFILE_EVENT_MASK_HIGH:-0x0}' "$root/scripts/buildstorm-profile-host.sh"
grep -q 'event_mask=${PROFILE_EVENT_MASK:-0xfef000000}' "$root/scripts/buildstorm-profile-guest.sh"
grep -q 'event_mask_high=${PROFILE_EVENT_MASK_HIGH:-0x0}' "$root/scripts/buildstorm-profile-guest.sh"

PROFILE_CONTROL="$control" \
PROFILE_STATS="$stats" \
PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_EVENT_MASK=0x0 \
PROFILE_EVENT_MASK_HIGH=0xffffffff \
    "$root/scripts/profile-capture.sh" start high-events >"$output"
[ "$(cat "$control")" = resume ]
grep -q '^control=timing_shift=8$' "$output"

PROFILE_CONTROL="$control" \
PROFILE_STATS="$stats" \
PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_TIMING_SHIFT=0 \
    "$root/scripts/profile-capture.sh" start exact >"$output"
grep -q '^control=timing_shift=0$' "$output"

PROFILE_CONTROL="$control" \
PROFILE_STATS="$stats" \
PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_TIMING_SHIFT=3 \
PROFILE_LEAVE_FROZEN=1 \
    "$root/scripts/profile-capture.sh" start gated >"$output"
[ "$(cat "$control")" = timing_shift=3 ]

printf 'sentinel\n' >"$control"
PROFILE_CONTROL="$control" \
PROFILE_STATS="$stats" \
PROFILE_SAMPLES="$tmp/missing-samples" \
PROFILE_TRACE_FILE="$trace" \
PROFILE_ALREADY_FROZEN=1 \
    "$root/scripts/profile-capture.sh" stop gated >"$output"
[ "$(cat "$control")" = sentinel ]

printf 'sentinel\n' >"$control"
if PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    PROFILE_TIMING_SHIFT=17 \
    "$root/scripts/profile-capture.sh" start invalid-shift >/dev/null 2>&1; then
    echo "profile-capture fixture: invalid timing shift was accepted" >&2
    exit 1
fi
[ "$(cat "$control")" = sentinel ]

if PROFILE_CONTROL="$control" \
    PROFILE_STATS="$stats" \
    "$root/scripts/profile-capture.sh" start 'bad case' >/dev/null 2>&1; then
    echo "profile-capture fixture: unsafe case id was accepted" >&2
    exit 1
fi
[ "$(cat "$control")" = sentinel ]

mkdir "$tmp/stats-directory"
: >"$output"
if PROFILE_CONTROL="$control" \
    PROFILE_STATS="$tmp/stats-directory" \
    PROFILE_SAMPLES="$tmp/missing-samples" \
    PROFILE_TRACE_FILE="$tmp/missing-trace" \
    "$root/scripts/profile-capture.sh" start unreadable >"$output" 2>/dev/null; then
    echo "profile-capture fixture: unreadable stats snapshot was accepted" >&2
    exit 1
fi
if grep -q '^@@PROFILE_STATS_BEGIN ' "$output"; then
    echo "profile-capture fixture: partial stats section was emitted" >&2
    exit 1
fi

echo "profile-capture fixture: ok"
