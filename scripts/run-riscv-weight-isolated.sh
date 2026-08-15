#!/bin/sh
set -eu

usage() {
    echo "usage: $0 <calibration-smoke|calibration-measure>" >&2
    exit 2
}

[ "$#" -eq 1 ] || usage
mode=$1
case "$mode" in calibration-smoke|calibration-measure) ;; *) usage ;; esac

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
selected_cpu=${RISCV_WEIGHT_CPUSET:-}
case "$selected_cpu" in ''|*[!0-9]*) echo "RISCV_WEIGHT_CPUSET must name one CPU" >&2; exit 2 ;; esac
[ "$(id -u)" -ne 0 ] || { echo "run the isolator as the measurement user, not root" >&2; exit 2; }
sudo -n true 2>/dev/null || { echo "passwordless sudo is required" >&2; exit 2; }
command -v systemd-run >/dev/null 2>&1 || { echo "systemd-run is required" >&2; exit 2; }

online=$(cat /sys/devices/system/cpu/online)
physical=$(cat "/sys/devices/system/cpu/cpu$selected_cpu/topology/thread_siblings_list")
background=$(python3 - "$online" "$physical" <<'PY'
import sys

def expand(spec):
    values = set()
    for part in spec.replace(",", " ").split():
        ends = [int(value) for value in part.split("-", 1)]
        values.update(range(ends[0], ends[-1] + 1))
    return values

def compact(values):
    result = []
    for value in sorted(values):
        if result and value == result[-1][-1] + 1:
            result[-1][-1] = value
        else:
            result.append([value, value])
    print(",".join(str(a) if a == b else f"{a}-{b}" for a, b in result))

compact(expand(sys.argv[1]) - expand(sys.argv[2]))
PY
)
[ -n "$background" ] || { echo "no CPUs remain for host services" >&2; exit 2; }
background_mask=$(python3 - "$background" <<'PY'
import sys

cpus = set()
for part in sys.argv[1].replace(",", " ").split():
    bounds = [int(value) for value in part.split("-", 1)]
    cpus.update(range(bounds[0], bounds[-1] + 1))
chunks = [0] * (max(cpus) // 32 + 1)
for cpu in cpus:
    chunks[cpu // 32] |= 1 << (cpu % 32)
high, *rest = reversed(chunks)
print(",".join([f"{high:x}", *(f"{value:08x}" for value in rest)]))
PY
)

orchestrator_cpu=${RISCV_WEIGHT_ORCHESTRATOR_CPU:-}
if [ -z "$orchestrator_cpu" ]; then
    for candidate in $(printf '%s\n' "$background" | tr ',' ' '); do
        case "$candidate" in *-*) candidate=${candidate%%-*} ;; esac
        candidate_siblings=$(cat "/sys/devices/system/cpu/cpu$candidate/topology/thread_siblings_list")
        [ "$candidate_siblings" = "$physical" ] || {
            orchestrator_cpu=$candidate
            break
        }
    done
fi
case "$orchestrator_cpu" in ''|*[!0-9]*) echo "no independent orchestrator CPU is available" >&2; exit 2 ;; esac
case ",$physical," in *",$orchestrator_cpu,"*) echo "orchestrator CPU shares the measured core" >&2; exit 2 ;; esac

stamp=$(date -u +%Y%m%dT%H%M%SZ)
output=${RISCV_WEIGHT_OUTPUT:-$root/build/riscv-instruction-weight-runs/isolated-$stamp}
case "$output" in "$root"/*) ;; *) echo "RISCV_WEIGHT_OUTPUT must be inside the repository" >&2; exit 2 ;; esac
mkdir -p "$output"
isolation_state=$output/isolation-state.json
restore_state=$output/isolation-restore.json
saved=$output/isolation-saved
mkdir -p "$saved"

slices="system.slice user.slice machine.slice measurement.slice"
for slice in $slices; do
    systemctl show --property=AllowedCPUs --value "$slice" >"$saved/$slice.allowed"
done
printf '%s\n' "$(cat /proc/irq/default_smp_affinity)" >"$saved/default_smp_affinity"
: >"$saved/irq-affinity.tsv"
for path in /proc/irq/*/smp_affinity_list; do
    [ -r "$path" ] || continue
    printf '%s\t%s\n' "$path" "$(cat "$path")" >>"$saved/irq-affinity.tsv"
done
kernel_affinity_plan=$saved/kernel-affinity-plan.tsv
python3 - "$kernel_affinity_plan" <<'PY'
import sys
from pathlib import Path

output = Path(sys.argv[1])
workqueue_root = Path("/sys/devices/virtual/workqueue")
required = {
    workqueue_root / "cpumask": ("global-workqueue", "mask"),
    workqueue_root / "writeback/cpumask": ("writeback-workqueue", "mask"),
    Path("/proc/sys/kernel/watchdog_cpumask"): ("watchdog", "list"),
}
paths = set(workqueue_root.glob("**/cpumask")) | {Path("/proc/sys/kernel/watchdog_cpumask")}
missing = sorted(str(path) for path in required if path not in paths or not path.is_file())
if missing:
    raise SystemExit("required kernel affinity controls are missing: " + ", ".join(missing))
rows = []
for path in sorted(paths, key=str):
    try:
        raw = path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise SystemExit(f"cannot read kernel affinity control {path}: {error}") from error
    if not raw or "\t" in raw or "\n" in raw:
        raise SystemExit(f"invalid kernel affinity value in {path}")
    if path in required:
        kind, syntax = required[path]
    else:
        kind, syntax = "named-workqueue", "mask"
    rows.append(f"{kind}\t{path}\t{syntax}\t{raw}")
output.write_text("\n".join(rows) + "\n", encoding="utf-8")
PY
: >"$saved/cpu-state.tsv"
for cpu in $(printf '%s\n' "$physical" | tr ',' ' '); do
    case "$cpu" in *-*) echo "non-enumerated SMT topology is unsupported" >&2; exit 2 ;; esac
    online_path=/sys/devices/system/cpu/cpu$cpu/online
    online_value=1
    [ ! -r "$online_path" ] || online_value=$(cat "$online_path")
    policy=/sys/devices/system/cpu/cpu$cpu/cpufreq
    printf '%s\t%s\t%s\t%s\t%s\n' "$cpu" "$online_value" \
        "$(cat "$policy/scaling_governor")" \
        "$(cat "$policy/scaling_min_freq")" \
        "$(cat "$policy/scaling_max_freq")" >>"$saved/cpu-state.tsv"
done

restored=0
restore_host() {
    [ "$restored" -eq 0 ] || return 0
    restored=1
    while IFS="$(printf '\t')" read -r cpu was_online governor minimum maximum; do
        online_path=/sys/devices/system/cpu/cpu$cpu/online
        [ "$was_online" -ne 1 ] || [ ! -e "$online_path" ] || \
            sudo sh -c "echo 1 > '$online_path'" || true
        policy=/sys/devices/system/cpu/cpu$cpu/cpufreq
        sudo sh -c "echo '$minimum' > '$policy/scaling_min_freq'" 2>/dev/null || true
        sudo sh -c "echo '$maximum' > '$policy/scaling_max_freq'" 2>/dev/null || true
        sudo sh -c "echo '$governor' > '$policy/scaling_governor'" 2>/dev/null || true
        [ "$was_online" -eq 1 ] || [ ! -e "$online_path" ] || \
            sudo sh -c "echo 0 > '$online_path'" || true
    done <"$saved/cpu-state.tsv"
    while IFS="$(printf '\t')" read -r path value; do
        sudo sh -c "echo '$value' > '$path'" 2>/dev/null || true
    done <"$saved/irq-affinity.tsv"
    while IFS="$(printf '\t')" read -r _kind path _syntax value; do
        sudo sh -c "echo '$value' > '$path'" 2>/dev/null || true
    done <"$kernel_affinity_plan"
    sudo sh -c "echo '$(cat "$saved/default_smp_affinity")' > /proc/irq/default_smp_affinity" 2>/dev/null || true
    for slice in $slices; do
        value=$(cat "$saved/$slice.allowed")
        sudo systemctl set-property --runtime "$slice" "AllowedCPUs=$value" >/dev/null 2>&1 || true
    done
    python3 - "$restore_state" "$selected_cpu" "$physical" \
        "$kernel_affinity_plan" <<'PY'
import json, sys, time
from pathlib import Path

def cpu_list(spec):
    result = set()
    for part in spec.replace(",", " ").split():
        bounds = [int(value) for value in part.split("-", 1)]
        result.update(range(bounds[0], bounds[-1] + 1))
    return result

def cpu_mask(spec):
    result = set()
    for chunk_index, chunk in enumerate(reversed(spec.split(","))):
        value = int(chunk, 16)
        for bit in range(32):
            if value & (1 << bit):
                result.add(chunk_index * 32 + bit)
    return result

failures = []
for line in Path(sys.argv[4]).read_text(encoding="utf-8").splitlines():
    kind, raw_path, syntax, initial_raw = line.split("\t", 3)
    path = Path(raw_path)
    try:
        restored_raw = path.read_text(encoding="utf-8").strip()
        parser = cpu_mask if syntax == "mask" else cpu_list
        if parser(restored_raw) != parser(initial_raw):
            failures.append({"kind": kind, "path": raw_path, "reason": "readback-mismatch"})
    except (OSError, ValueError) as error:
        failures.append({"kind": kind, "path": raw_path, "reason": str(error)})
Path(sys.argv[1]).write_text(json.dumps({
    "schema": "mygo.riscv-weight-host-isolation-restore.v2",
    "restored_at_ns": time.time_ns(),
    "selected_cpu": int(sys.argv[2]),
    "physical_core_cpuset": sys.argv[3],
    "restore_attempted": True,
    "kernel_affinity_restore_verified": not failures,
    "kernel_affinity_restore_failures": failures,
}, sort_keys=True, indent=2) + "\n", encoding="utf-8")
PY
}
trap restore_host EXIT HUP INT TERM

kernel_affinity_failure_details=$saved/kernel-affinity-apply-failures.tsv
: >"$kernel_affinity_failure_details"
while IFS="$(printf '\t')" read -r _kind path syntax _initial; do
    case "$syntax" in
        mask) requested=$background_mask ;;
        list) requested=$background ;;
        *) echo "invalid kernel affinity syntax: $syntax" >&2; exit 1 ;;
    esac
    error_file=$saved/kernel-affinity-write-error
    if sudo env LC_ALL=C sh -c "echo '$requested' > '$path'" 2>"$error_file"; then
        :
    else
        status=$?
        error=$(tr '\t\n' '  ' <"$error_file")
        printf '%s\t%s\t%s\n' "$path" "$status" "$error" \
            >>"$kernel_affinity_failure_details"
    fi
done <"$kernel_affinity_plan"

for slice in system.slice user.slice machine.slice; do
    sudo systemctl set-property --runtime "$slice" "AllowedCPUs=$background"
done
sudo systemctl set-property --runtime measurement.slice \
    "AllowedCPUs=$background,$selected_cpu"

irq_failures=0
irq_default_write_failed=0
irq_error_log=$output/irq-affinity-apply-errors.log
irq_plan=$saved/irq-plan.tsv
irq_initial_read_errors=$saved/irq-initial-read-errors
irq_attempted_paths=$saved/irq-attempted-paths
irq_failed_paths=$saved/irq-failed-paths
irq_failure_details=$saved/irq-failures.tsv
: >"$irq_error_log"
: >"$irq_initial_read_errors"
: >"$irq_attempted_paths"
: >"$irq_failed_paths"
: >"$irq_failure_details"
python3 - "$physical" "$irq_plan" "$irq_initial_read_errors" <<'PY'
import sys
from pathlib import Path

def expand(spec):
    result = set()
    for part in spec.replace(",", " ").split():
        bounds = [int(value) for value in part.split("-", 1)]
        result.update(range(bounds[0], bounds[-1] + 1))
    return result

physical = expand(sys.argv[1])
rows = []
errors = []
for affinity in sorted(
    Path("/proc/irq").glob("*/smp_affinity_list"),
    key=lambda item: int(item.parent.name),
):
    effective = affinity.with_name("effective_affinity_list")
    actions_path = Path("/sys/kernel/irq") / affinity.parent.name / "actions"
    try:
        requested_raw = affinity.read_text(encoding="utf-8").strip()
        requested = expand(requested_raw)
        effective_raw = effective.read_text(encoding="utf-8").strip()
        effective_cpus = expand(effective_raw) if effective_raw else set()
        actions = actions_path.read_text(encoding="utf-8").strip()
    except (OSError, ValueError) as error:
        errors.append(f"{affinity}\t{error}")
        continue
    actions = actions.replace("\t", " ").replace("\n", " ")
    if actions and not effective_cpus:
        classification = "invalid_active_no_target"
        errors.append(f"{affinity}\tactive IRQ has no effective target")
    elif not actions and not effective_cpus:
        classification = "inactive_no_target"
    elif effective_cpus & physical:
        classification = "needs_migration"
    else:
        classification = "already_excluded"
    rows.append(
        "\t".join(
            (
                str(affinity),
                str(int(classification == "needs_migration")),
                effective_raw,
                requested_raw,
                actions,
                classification,
            )
        )
    )
Path(sys.argv[2]).write_text("\n".join(rows) + "\n", encoding="utf-8")
Path(sys.argv[3]).write_text("\n".join(errors) + ("\n" if errors else ""), encoding="utf-8")
PY
if ! sudo env LC_ALL=C sh -c "echo '$background_mask' > /proc/irq/default_smp_affinity" \
    2>>"$irq_error_log"; then
    irq_default_write_failed=1
    irq_failures=$((irq_failures + 1))
fi
while IFS="$(printf '\t')" read -r path migration_required _initial_e _initial_r _initial_a _initial_class; do
    [ -n "$path" ] || continue
    [ "$migration_required" -eq 1 ] || continue
    printf '%s\n' "$path" >>"$irq_attempted_paths"
    irq_write_error=$saved/irq-write-error
    if sudo env LC_ALL=C sh -c "echo '$background' > '$path'" \
        2>"$irq_write_error"; then
        :
    else
        status=$?
        printf '%s\n' "$path" >>"$irq_failed_paths"
        error=$(tr '\t\n' '  ' <"$irq_write_error")
        printf '%s\t%s\t%s\n' "$path" "$status" "$error" \
            >>"$irq_failure_details"
        cat "$irq_write_error" >>"$irq_error_log"
        irq_failures=$((irq_failures + 1))
    fi
done <"$irq_plan"

while IFS="$(printf '\t')" read -r cpu _was_online _governor _minimum _maximum; do
    policy=/sys/devices/system/cpu/cpu$cpu/cpufreq
    maximum=$(cat "$policy/cpuinfo_max_freq")
    sudo sh -c "echo performance > '$policy/scaling_governor'"
    sudo sh -c "echo '$maximum' > '$policy/scaling_max_freq'"
    sudo sh -c "echo '$maximum' > '$policy/scaling_min_freq'"
    if [ "$cpu" -ne "$selected_cpu" ]; then
        online_path=/sys/devices/system/cpu/cpu$cpu/online
        [ ! -e "$online_path" ] || sudo sh -c "echo 0 > '$online_path'"
    fi
done <"$saved/cpu-state.tsv"

python3 - "$isolation_state" "$selected_cpu" "$physical" "$orchestrator_cpu" \
    "$background" "$background_mask" "$irq_failures" "$irq_default_write_failed" \
    "$irq_plan" "$irq_initial_read_errors" \
    "$irq_attempted_paths" "$irq_failed_paths" "$irq_failure_details" \
    "$kernel_affinity_plan" "$kernel_affinity_failure_details" <<'PY'
import hashlib, json, subprocess, sys, time
from pathlib import Path

(
    path,
    selected,
    physical,
    orchestrator,
    background,
    background_mask,
    irq_failures,
    irq_default_write_failed,
    irq_plan,
    irq_initial_read_errors,
    irq_attempted_paths,
    irq_failed_paths,
    irq_failure_details,
    kernel_affinity_plan,
    kernel_affinity_failure_details,
) = sys.argv[1:]
selected_cpu = int(selected)
orchestrator_cpu = int(orchestrator)

def expand(spec):
    values = set()
    for part in spec.replace(",", " ").split():
        bounds = [int(value) for value in part.split("-", 1)]
        values.update(range(bounds[0], bounds[-1] + 1))
    return values

def parse_cpumask(spec):
    chunks = spec.strip().split(",")
    if not chunks or any(not chunk for chunk in chunks):
        raise ValueError(f"invalid empty cpumask: {spec!r}")
    result = set()
    for chunk_index, chunk in enumerate(reversed(chunks)):
        value = int(chunk, 16)
        bit = 0
        while value:
            if value & 1:
                result.add(chunk_index * 32 + bit)
            value >>= 1
            bit += 1
    return result

def read(path):
    path = Path(path)
    last_error = None
    for attempt in range(5):
        try:
            return path.read_text(encoding="utf-8").strip()
        except OSError as error:
            last_error = error
            if error.errno not in {11, 16} or attempt == 4:
                raise
            time.sleep(0.02 * (attempt + 1))
    assert last_error is not None
    raise last_error

def systemctl_allowed(name):
    result = subprocess.run(
        ["systemctl", "show", "--property=EffectiveCPUs", "--value", name],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    value = result.stdout.strip()
    if not value:
        raise ValueError(f"{name} has no EffectiveCPUs readback")
    return value

def canonical_sha256(value):
    payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()

physical_cpus = sorted(expand(physical))
physical_set = set(physical_cpus)
background_set = expand(background)
online_cpus = expand(read("/sys/devices/system/cpu/online"))
slices = {}
for name in ("system.slice", "user.slice", "machine.slice"):
    allowed = systemctl_allowed(name)
    effective = expand(allowed)
    slices[name] = {
        "effective_cpus": allowed,
        "effective_cpu_list": sorted(effective),
        "is_subset_of_requested_background": bool(effective)
        and effective.issubset(background_set),
        "excludes_physical_core": bool(effective & physical_set) is False,
    }
measurement_allowed = systemctl_allowed("measurement.slice")
measurement_effective = expand(measurement_allowed)
measurement_requested = background_set | {selected_cpu}
sibling_states = {
    str(cpu): cpu in online_cpus
    for cpu in physical_cpus
}
frequency = {}
for cpu in physical_cpus:
    if not sibling_states[str(cpu)]:
        frequency[str(cpu)] = None
        continue
    policy = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
    try:
        frequency[str(cpu)] = {
            "governor": read(policy / "scaling_governor"),
            "minimum": int(read(policy / "scaling_min_freq")),
            "maximum": int(read(policy / "scaling_max_freq")),
            "cpuinfo_maximum": int(read(policy / "cpuinfo_max_freq")),
        }
    except (FileNotFoundError, ValueError):
        frequency[str(cpu)] = None
frequency_applied = all(
    isinstance(row, dict)
    and row["governor"] == "performance"
    and row["minimum"] == row["cpuinfo_maximum"]
    and row["maximum"] == row["cpuinfo_maximum"]
    for cpu, row in frequency.items()
    if sibling_states[cpu]
)

write_failures = {}
for line in Path(kernel_affinity_failure_details).read_text(
    encoding="utf-8"
).splitlines():
    failed_path, status, message = line.split("\t", 2)
    write_failures[failed_path] = {"status": int(status), "message": message}
kernel_affinity_entries = []
for line in Path(kernel_affinity_plan).read_text(encoding="utf-8").splitlines():
    kind, raw_path, syntax, initial_raw = line.split("\t", 3)
    parser = parse_cpumask if syntax == "mask" else expand
    requested_raw = background_mask if syntax == "mask" else background
    entry = {
        "kind": kind,
        "path": raw_path,
        "syntax": syntax,
        "initial_raw": initial_raw,
        "initial_cpus": sorted(parser(initial_raw)),
        "requested_raw": requested_raw,
        "requested_cpus": sorted(parser(requested_raw)),
        "write_attempted": True,
        "write_failed": raw_path in write_failures,
        "write_error": write_failures.get(raw_path),
    }
    try:
        readback_raw = read(raw_path)
        readback_cpus = parser(readback_raw)
        entry["readback_raw"] = readback_raw
        entry["readback_cpus"] = sorted(readback_cpus)
        entry["matches_requested"] = readback_cpus == background_set
        entry["excludes_physical_core"] = not bool(readback_cpus & physical_set)
    except (OSError, ValueError) as error:
        entry["readback_error"] = str(error)
        entry["matches_requested"] = False
        entry["excludes_physical_core"] = False
    kernel_affinity_entries.append(entry)
kernel_affinity_required_kinds = {
    "global-workqueue", "writeback-workqueue", "watchdog"
}
kernel_affinity_policy_satisfied = (
    kernel_affinity_required_kinds.issubset(
        {entry["kind"] for entry in kernel_affinity_entries}
    )
    and not write_failures
    and all(
        entry["matches_requested"] and entry["excludes_physical_core"]
        for entry in kernel_affinity_entries
    )
)

initial = {}
for line in Path(irq_plan).read_text(encoding="utf-8").splitlines():
    (
        irq_path,
        required,
        effective_raw,
        requested_raw,
        actions,
        classification,
    ) = line.split("\t", 5)
    initial[irq_path] = {
        "migration_required": required == "1",
        "effective_raw": effective_raw,
        "effective_cpus": sorted(expand(effective_raw)) if effective_raw else [],
        "requested_raw": requested_raw,
        "requested_cpus": sorted(expand(requested_raw)),
        "actions": actions,
        "classification": classification,
    }
initial_read_errors = Path(irq_initial_read_errors).read_text(
    encoding="utf-8"
).splitlines()
attempted_paths = set(
    Path(irq_attempted_paths).read_text(encoding="utf-8").splitlines()
)
failed_paths = set(Path(irq_failed_paths).read_text(encoding="utf-8").splitlines())
write_errors = {}
for line in Path(irq_failure_details).read_text(encoding="utf-8").splitlines():
    irq_path, status, error = line.split("\t", 2)
    write_errors[irq_path] = {"status": int(status), "message": error}
irq_readback_failures = []
irq_violations = []
irq_entries = []
observed_paths = set()
for affinity_path in sorted(
    Path("/proc/irq").glob("*/smp_affinity_list"),
    key=lambda item: int(item.parent.name),
):
    observed_paths.add(str(affinity_path))
    entry = {
        "irq": int(affinity_path.parent.name),
        "path": str(affinity_path),
        "appeared_after_plan": str(affinity_path) not in initial,
        "migration_required": initial.get(str(affinity_path), {}).get(
            "migration_required", False
        ),
        "write_attempted": str(affinity_path) in attempted_paths,
        "write_failed": str(affinity_path) in failed_paths,
        "write_error": write_errors.get(str(affinity_path)),
    }
    actions_path = Path("/sys/kernel/irq") / affinity_path.parent.name / "actions"
    try:
        entry["actions"] = read(actions_path)
    except OSError:
        entry["actions"] = None
    if str(affinity_path) in initial:
        entry["initial_effective_raw"] = initial[str(affinity_path)][
            "effective_raw"
        ]
        entry["initial_effective_cpus"] = initial[str(affinity_path)][
            "effective_cpus"
        ]
        entry["initial_requested_raw"] = initial[str(affinity_path)][
            "requested_raw"
        ]
        entry["initial_requested_cpus"] = initial[str(affinity_path)][
            "requested_cpus"
        ]
        entry["initial_actions"] = initial[str(affinity_path)]["actions"]
        entry["initial_classification"] = initial[str(affinity_path)][
            "classification"
        ]
    try:
        requested_raw = read(affinity_path)
        requested = expand(requested_raw)
        effective_path = affinity_path.with_name("effective_affinity_list")
        effective_raw = read(effective_path)
        effective = expand(effective_raw) if effective_raw else set()
        entry["requested_raw"] = requested_raw
        entry["requested_cpus"] = sorted(requested)
        entry["effective_path"] = str(effective_path)
        entry["effective_raw"] = effective_raw
        entry["effective_cpus"] = sorted(effective)
    except (OSError, ValueError) as error:
        entry["readback_error"] = str(error)
        irq_readback_failures.append(str(affinity_path))
        irq_entries.append(entry)
        continue
    reasons = []
    if entry["appeared_after_plan"]:
        reasons.append("irq-appeared-after-plan")
    elif entry["actions"] != entry["initial_actions"]:
        reasons.append("irq-actions-changed-after-plan")
    elif not entry["actions"] and not effective:
        if entry["initial_classification"] != "inactive_no_target":
            reasons.append("irq-lost-action-and-effective-target")
        elif entry["write_attempted"] or entry["write_failed"]:
            reasons.append("inactive-irq-migration-was-attempted")
        else:
            entry["classification"] = "inactive_no_target"
    elif entry["actions"] and not effective:
        reasons.append("active-irq-has-no-effective-target")
    elif entry["migration_required"]:
        if not entry["write_attempted"]:
            reasons.append("required-migration-not-attempted")
        elif (
            effective & physical_set
            and isinstance(entry["actions"], str)
            and bool(entry["actions"])
        ):
            # Managed/per-CPU IRQs may either reject the affinity write or
            # accept it without changing the effective target.  The readback,
            # rather than a locale-dependent errno string, determines whether
            # the IRQ remains on the measured physical core.
            entry["classification"] = "residual_unmigratable"
        else:
            # CPU hotplug may migrate a managed IRQ even when the explicit
            # affinity write reports failure.  Preserve the write evidence,
            # but let the effective-affinity readback decide isolation.
            entry["classification"] = "migrated_and_verified"
    elif entry["write_attempted"]:
        reasons.append("unexpected-migration-attempt")
    elif effective & physical_set:
        reasons.append("excluded-irq-drifted-to-physical-core")
    else:
        entry["classification"] = "already_excluded"
    if reasons:
        entry["classification"] = "violation"
        irq_violations.append({"path": str(affinity_path), "reasons": reasons})
    irq_entries.append(entry)

irq_disappeared_after_plan = sorted(set(initial) - observed_paths)
irq_appeared_after_plan = sorted(observed_paths - set(initial))

default_affinity_raw = read("/proc/irq/default_smp_affinity")
default_affinity_cpus = parse_cpumask(default_affinity_raw)
default_affinity_matches = default_affinity_cpus == background_set
residual_irqs = [
    {
        "irq": entry["irq"],
        "path": entry["path"],
        "actions": entry["actions"],
        "effective_cpus": entry["effective_cpus"],
        "write_error": entry["write_error"],
    }
    for entry in irq_entries
    if entry.get("classification") == "residual_unmigratable"
]
irq_policy_satisfied = (
    not bool(int(irq_default_write_failed))
    and not initial_read_errors
    and not irq_readback_failures
    and not irq_violations
    and not irq_disappeared_after_plan
    and not irq_appeared_after_plan
    and default_affinity_matches
    and bool(irq_entries)
)
irq_snapshot = json.dumps(irq_entries, sort_keys=True, separators=(",", ":"))
residual_snapshot = json.dumps(
    residual_irqs, sort_keys=True, separators=(",", ":")
)

state = {
    "schema": "mygo.riscv-weight-host-isolation.v4",
    "captured_at_ns": time.time_ns(),
    "active_during_measurement": False,
    "selected_cpus": [selected_cpu],
    "physical_core_cpus": physical_cpus,
    "orchestrator_cpu": orchestrator_cpu,
    "requested_background_cpus": sorted(background_set),
    "online_cpus": sorted(online_cpus),
    "selected_cpu_online": selected_cpu in online_cpus,
    "orchestrator_cpu_online": orchestrator_cpu in online_cpus,
    "smt_sibling_online_states": sibling_states,
    "smt_siblings_offline": all(
        cpu == selected_cpu or not sibling_states[str(cpu)]
        for cpu in physical_cpus
    ),
    "measurement_slice_active": measurement_effective == measurement_requested,
    "measurement_slice_effective_cpus": measurement_allowed,
    "measurement_slice_effective_cpu_list": sorted(measurement_effective),
    "background_slices": slices,
    "frequency": frequency,
    "frequency_policy_applied": frequency_applied,
    "kernel_affinity_entries": kernel_affinity_entries,
    "kernel_affinity_entries_sha256": canonical_sha256(kernel_affinity_entries),
    "kernel_affinity_observed_count": len(kernel_affinity_entries),
    "kernel_affinity_write_failure_count": len(write_failures),
    "kernel_affinity_failed_paths": sorted(write_failures),
    "kernel_affinity_required_kinds": sorted(kernel_affinity_required_kinds),
    "kernel_affinity_policy_satisfied": kernel_affinity_policy_satisfied,
    "irq_affinity_attempt_failures": int(irq_failures),
    "irq_affinity_default_write_failed": irq_default_write_failed == "1",
    "irq_affinity_initial_read_errors": initial_read_errors,
    "irq_affinity_readback_failures": irq_readback_failures,
    "irq_affinity_disappeared_after_plan": irq_disappeared_after_plan,
    "irq_affinity_appeared_after_plan": irq_appeared_after_plan,
    "irq_affinity_violations": irq_violations,
    "irq_affinity_default_raw": default_affinity_raw,
    "irq_affinity_default_effective_cpus": sorted(default_affinity_cpus),
    "irq_affinity_default_matches_requested": default_affinity_matches,
    "irq_affinity_observed_count": len(irq_entries),
    "irq_affinity_planned_count": len(initial),
    "irq_affinity_migration_required_count": sum(
        item["migration_required"] for item in initial.values()
    ),
    "irq_affinity_write_attempt_count": len(attempted_paths),
    "irq_affinity_attempted_paths": sorted(attempted_paths),
    "irq_affinity_write_failure_count": len(failed_paths),
    "irq_affinity_failed_paths": sorted(failed_paths),
    "irq_affinity_skipped_safe_count": sum(
        not item["migration_required"] for item in initial.values()
    ),
    "irq_affinity_readback_violation_count": len(irq_violations),
    "irq_affinity_entries_sha256": hashlib.sha256(irq_snapshot.encode()).hexdigest(),
    "irq_affinity_entries": irq_entries,
    "irq_affinity_migrated_and_verified_count": sum(
        entry.get("classification") == "migrated_and_verified"
        for entry in irq_entries
    ),
    "irq_affinity_already_excluded_count": sum(
        entry.get("classification") == "already_excluded"
        for entry in irq_entries
    ),
    "irq_affinity_inactive_no_target_count": sum(
        entry.get("classification") == "inactive_no_target"
        for entry in irq_entries
    ),
    "irq_affinity_residual_unmigratable_count": len(residual_irqs),
    "irq_affinity_residual_unmigratable": residual_irqs,
    "irq_affinity_residual_unmigratable_sha256": hashlib.sha256(
        residual_snapshot.encode()
    ).hexdigest(),
    "irq_affinity_applied": not residual_irqs and irq_policy_satisfied,
    "irq_isolation_policy_satisfied": irq_policy_satisfied,
    "irq_residual_requires_zero_external_interrupts": bool(residual_irqs),
    "restore_trap_armed": True,
}
checks = {
    "selected_cpu_online": state["selected_cpu_online"],
    "orchestrator_cpu_online": state["orchestrator_cpu_online"],
    "smt_siblings_offline": state["smt_siblings_offline"],
    "measurement_slice_active": state["measurement_slice_active"],
    "background_slices_applied": all(
        item["is_subset_of_requested_background"]
        and item["excludes_physical_core"]
        for item in slices.values()
    ),
    "frequency_policy_applied": state["frequency_policy_applied"],
    "irq_isolation_policy_satisfied": state["irq_isolation_policy_satisfied"],
    "kernel_affinity_policy_satisfied": state[
        "kernel_affinity_policy_satisfied"
    ],
}
state["preflight_checks"] = checks
state["active_during_measurement"] = all(checks.values())
Path(path).write_text(
    json.dumps(state, sort_keys=True, indent=2) + "\n", encoding="utf-8"
)
failed = [name for name, passed in checks.items() if not passed]
if failed:
    raise SystemExit("host isolation readback failed: " + ", ".join(failed))
PY

set -- sudo systemd-run --wait --pipe --collect --uid="$(id -u)" --gid="$(id -g)" \
    --slice=measurement.slice -p "AllowedCPUs=$background,$selected_cpu" \
    -p "CPUAffinity=$background"
for name in RISCV_WEIGHT_RUNS RISCV_WEIGHT_BASE_BLOCKS RISCV_WEIGHT_ROUNDS \
    RISCV_WEIGHT_BOOTSTRAP RISCV_WEIGHT_BOOTSTRAP_JOBS \
    RISCV_WEIGHT_LINEAR_ALGEBRA_BACKEND RISCV_WEIGHT_ML_FOLDS \
    RISCV_WEIGHT_ML_MAX_ITER RISCV_WEIGHT_ML_BOOTSTRAP \
    RISCV_WEIGHT_ML_MINIMUM_RUNS RISCV_WEIGHT_TIMEOUT \
    RISCV_WEIGHT_STARTUP_WARMUPS RISCV_WEIGHT_SKIP_BUILD; do
    value=$(printenv "$name" 2>/dev/null || true)
    [ -z "$value" ] || set -- "$@" -E "$name=$value"
done
set -- "$@" -E "HOME=$HOME" -E "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
    -E "RISCV_WEIGHT_OUTPUT=$output" -E "RISCV_WEIGHT_CPUSET=$selected_cpu" \
    -E "RISCV_WEIGHT_PHYSICAL_CORE_CPUSET=$physical" \
    -E "RISCV_WEIGHT_CPUSET_MODE=taskset" -E "RISCV_WEIGHT_REQUIRE_EXCLUSIVE_CPU=1" \
    -E "RISCV_WEIGHT_HOST_TELEMETRY_SUDO=1" \
    -E "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_WINDOW_FREQUENCY=1" \
    -E "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_FREQUENCY_PREFLIGHT=1" \
    -E "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_INTERRUPTS=1" \
    -E "RISCV_WEIGHT_HOST_AUDIT_REQUIRE_SCHEDSTAT=1" \
    -E "RISCV_WEIGHT_HOST_AUDIT_MAX_INTERRUPTS_PER_SECOND=0" \
    -E "RISCV_WEIGHT_HOST_AUDIT_MAX_RUNQUEUE_WAIT_FRACTION=0.01" \
    -E "RISCV_WEIGHT_REQUIRE_ISOLATION_STATE=1" \
    -E "RISCV_WEIGHT_ISOLATION_STATE=$isolation_state" \
    -E "RISCV_WEIGHT_CONTAINER_RUNTIME=${RISCV_WEIGHT_CONTAINER_RUNTIME:-podman}" \
    -E "RISCV_WEIGHT_CONTAINER_RUN_ARGUMENTS=${RISCV_WEIGHT_CONTAINER_RUN_ARGUMENTS:---cgroup-manager=cgroupfs --cgroups=disabled}" \
    -E "RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX=${RISCV_WEIGHT_CONTAINER_MOUNT_SUFFIX:-:z}" \
    /bin/sh "$root/scripts/riscv-instruction-weight.sh" "$mode"

"$@"
