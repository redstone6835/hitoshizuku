#!/bin/sh
set -eu

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <serial-log> <serial-log> [...]" >&2
    exit 2
fi

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
env_rows=$tmp/environment
span_rows=$tmp/spans
: >"$env_rows"
: >"$span_rows"

run=0
for log in "$@"; do
    run=$((run + 1))
    if [ ! -r "$log" ]; then
        echo "profile campaign: unreadable log: $log" >&2
        exit 1
    fi
    report=$tmp/report-$run
    "$root/scripts/profile-analyze.sh" "$log" 1000000 >"$report"

    tr -d '\r' <"$log" | awk -v run="$run" '
    function value(name,    i, pair) {
        for (i = 1; i <= NF; i++) {
            split($i, pair, "=")
            if (pair[1] == name) return pair[2]
        }
        return ""
    }
    /^@@PROFILE_META_BEGIN / {
        case_id = value("case")
        active = value("phase") == "after"
        next
    }
    /^@@PROFILE_META_END / { active = 0; next }
    active && /^(arch|cpu_online|kernel_image_id|rootfs_image_id|workload|cmdline)=/ {
        name = $0
        sub(/=.*/, "", name)
        contents = $0
        sub(/^[^=]*=/, "", contents)
        print run "\t" case_id "\t" name "\t" contents
    }
    ' >>"$env_rows"

    awk -F '\t' -v run="$run" '
    $0 == "WORKLOAD_SPANS" { active = 1; header = 1; next }
    active && $0 == "" { exit }
    active && header { header = 0; next }
    active && $3 != "" { print run "\t" $1 "\t" $3 "\t" $6 }
    ' "$report" >>"$span_rows"
done

awk -F '\t' '
{
    key = $2 SUBSEP $3
    if (!(key in baseline)) {
        baseline[key] = $4
        baseline_run[key] = $1
    } else if (baseline[key] != $4) {
        printf "profile campaign: environment mismatch case=%s field=%s run=%s baseline_run=%s\n", \
            $2, $3, $1, baseline_run[key] > "/dev/stderr"
        invalid = 1
    }
}
END { if (invalid) exit 1 }
' "$env_rows"

if [ ! -s "$span_rows" ]; then
    echo "profile campaign: no workload syscall spans" >&2
    exit 1
fi

echo "PROFILE_CAMPAIGN version=1 logs=$run"
echo "ENVIRONMENT"
printf 'case\tfield\tvalue\n'
awk -F '\t' '!seen[$2 SUBSEP $3]++ { print $2 "\t" $3 "\t" $4 }' "$env_rows" | sort

echo
echo "WORKLOAD_SYSCALL_CAMPAIGN"
printf 'case\tsyscall\truns\tspans\tmean_us\tmedian_us\tp95_us\tstddev_us\tcv_pct\tmin_us\tmax_us\n'
cut -f2,3 "$span_rows" | sort -u | while IFS="$(printf '\t')" read -r case_id syscall; do
    values=$tmp/values
    awk -F '\t' -v case_id="$case_id" -v syscall="$syscall" '
        $2 == case_id && $3 == syscall { print $4 }
    ' "$span_rows" | sort -n >"$values"
    runs=$(awk -F '\t' -v case_id="$case_id" -v syscall="$syscall" '
        $2 == case_id && $3 == syscall { seen[$1] = 1 }
        END { for (run in seen) count++; print count + 0 }
    ' "$span_rows")
    awk -v case_id="$case_id" -v syscall="$syscall" -v runs="$runs" '
    {
        value[NR] = $1
        total += $1
    }
    END {
        if (NR > 0) {
            mean = total / NR
            for (i = 1; i <= NR; i++) squared += (value[i] - mean) ^ 2
            stddev = NR > 1 ? sqrt(squared / (NR - 1)) : 0
            median = NR % 2 ? value[(NR + 1) / 2] : (value[NR / 2] + value[NR / 2 + 1]) / 2
            p95 = value[int((NR * 95 + 99) / 100)]
            cv = mean != 0 ? stddev * 100 / mean : 0
            printf "%s\t%s\t%d\t%d\t%.3f\t%.3f\t%.3f\t%.3f\t%.1f\t%.3f\t%.3f\n", \
                case_id, syscall, runs, NR, mean, median, p95, stddev, cv, value[1], value[NR]
        }
    }
    ' "$values"
done
