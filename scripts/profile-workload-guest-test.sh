#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
control=$tmp/profile_control
snapshot=$tmp/profile_snapshot
health=$tmp/profile_health
source_script=$tmp/workload.sh
phase_rules=$tmp/phases.tsv
output_root=$tmp/output
: >"$control"
printf 'snapshot-fixture\n' >"$snapshot"
printf 'valid=1 complete=1 samples_complete=1 trace_complete=1 errno_complete=1 tasks_complete=1 state=frozen active_writers=0 dropped_samples=0 dropped_trace=0 dropped_errno_records=0 dropped_task_records=0 schema_version=1 snapshot_bytes=17\n' >"$health"

cat >"$source_script" <<'EOF'
#!/bin/sh
echo "prepare workload"
echo "execute workload"
echo "finish workload"
EOF
printf '1\tprepare\t^echo "prepare workload"\n2\texecute\t^echo "execute workload"\n3\tfinish\t^echo "finish workload"\n' >"$phase_rules"
chmod 0755 "$source_script"

PROFILE_CONTROL="$control" \
PROFILE_SNAPSHOT="$snapshot" \
PROFILE_HEALTH="$health" \
PROFILE_OUTPUT_ROOT="$output_root" \
    "$root/scripts/profile-workload-guest.sh" fixture "$source_script" "$phase_rules" \
    >"$tmp/serial.log"

for phase in initial prepare execute finish; do grep -q "name=$phase" "$tmp/serial.log"; done
grep -Eq '^@@PROFILE_WORKLOAD case=fixture pid=[0-9]+$' "$tmp/serial.log"
grep -q '^@@PROFILE_ARTIFACT ' "$tmp/serial.log"
grep -q '^snapshot-fixture$' "$output_root/fixture-$(uname -m).bin"
grep -q '^valid=1 ' "$output_root/fixture-$(uname -m).health"
[ "$(cat "$control")" = freeze ]
echo "profile-workload-guest fixture: ok"
