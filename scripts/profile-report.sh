#!/bin/sh
set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <serial-log> [kernel-elf]" >&2
    exit 2
fi

log=$1
elf=${2:-}
if [ -z "$elf" ]; then
    run_dir=$(dirname "$(dirname "$log")")
    case $(basename "$log") in
        la-*) target=loongarch64-unknown-none ;;
        rv-*) target=riscv64gc-unknown-none-elf ;;
        *) target= ;;
    esac
    [ -z "$target" ] || elf="$run_dir/target/$target/release/kernel"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
clean_log="$tmp/log"
tr -d '\r' <"$log" >"$clean_log"

echo "EVENTS"
awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return 0
}
function percentile(key, before, pct,    i, total, target, seen, count) {
    total = 0
    for (i = 0; i < 32; i++) total += hist[key, i] - hist[before, i]
    if (total <= 0) return 0
    target = int((total * pct + 99) / 100)
    seen = 0
    for (i = 0; i < 32; i++) {
        count = hist[key, i] - hist[before, i]
        seen += count
        if (seen >= target) return i == 0 ? 0 : 2 ^ (i - 1)
    }
    return 2 ^ 30
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_STATS_END / { active = 0; next }
active && /^cpu=/ && / event=/ {
    event = value("event")
    key = case_id SUBSEP event SUBSEP phase
    calls[key] += value("calls")
    cycles[key] += value("cycles")
    bytes[key] += value("bytes")
    packets[key] += value("packets")
    wall[key] += value("wall_ns")
    oncpu[key] += value("on_cpu_ns")
    offcpu[key] += value("off_cpu_ns")
    split(value("hist"), buckets, ",")
    for (i = 1; i <= 32; i++) hist[key, i - 1] += buckets[i]
    observed[key] = 1
    next
}
END {
    print "case\tevent\tcalls\tcycles\tbytes\tpackets\twall_ns\ton_cpu_ns\toff_cpu_ns\toff_cpu%\tp50_ns\tp95_ns\tp99_ns"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        dcalls = calls[key] - calls[before]
        dcycles = cycles[key] - cycles[before]
        dbytes = bytes[key] - bytes[before]
        dpackets = packets[key] - packets[before]
        dwall = wall[key] - wall[before]
        don = oncpu[key] - oncpu[before]
        doff = offcpu[key] - offcpu[before]
        offpct = dwall ? doff * 100 / dwall : 0
        printf "%s\t%s\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.0f\t%.1f\t%.0f\t%.0f\t%.0f\n", \
            parts[1], parts[2], dcalls, dcycles, dbytes, dpackets, \
            dwall, don, doff, offpct, percentile(key, before, 50), \
            percentile(key, before, 95), percentile(key, before, 99)
    }
}
' "$clean_log" | {
    IFS= read -r header
    printf '%s\n' "$header"
    sort
}

echo
echo "METRICS"
awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return 0
}
function percentile(key, before, pct,    i, total, target, seen, count) {
    total = 0
    for (i = 0; i < 32; i++) total += hist[key, i] - hist[before, i]
    if (total <= 0) return 0
    target = int((total * pct + 99) / 100)
    seen = 0
    for (i = 0; i < 32; i++) {
        count = hist[key, i] - hist[before, i]
        seen += count
        if (seen >= target) return i == 0 ? 0 : 2 ^ (i - 1)
    }
    return 2 ^ 30
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_STATS_END / { active = 0; next }
active && /^cpu=/ && / metric=/ {
    metric = value("metric")
    key = case_id SUBSEP metric SUBSEP phase
    observations[key] += value("observations")
    sum[key] += value("sum")
    if (value("max") > max[key]) max[key] = value("max")
    split(value("hist"), buckets, ",")
    for (i = 1; i <= 32; i++) hist[key, i - 1] += buckets[i]
    observed[key] = 1
}
END {
    print "case\tmetric\tobservations\tsum\tmean\tmax\tp50\tp95\tp99"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        count = observations[key] - observations[before]
        total = sum[key] - sum[before]
        mean = count ? total / count : 0
        printf "%s\t%s\t%.0f\t%.0f\t%.2f\t%.0f\t%.0f\t%.0f\t%.0f\n", \
            parts[1], parts[2], count, total, mean, max[key], \
            percentile(key, before, 50), percentile(key, before, 95), \
            percentile(key, before, 99)
    }
}
' "$clean_log" | {
    IFS= read -r header
    printf '%s\n' "$header"
    sort
}

awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
/^@@PROFILE_SAMPLES_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = phase == "after"
    next
}
/^@@PROFILE_SAMPLES_END / { active = 0; next }
active && /^cpu=/ && / mode=/ {
    key = case_id SUBSEP value("mode") SUBSEP value("pc")
    samples[key] += value("samples")
}
END {
    for (key in samples) {
        split(key, parts, SUBSEP)
        print parts[1] "\t" parts[2] "\t" parts[3] "\t" samples[key]
    }
}
' "$clean_log" >"$tmp/samples"

if [ ! -s "$tmp/samples" ]; then
    exit 0
fi

echo
echo "PC SAMPLES"

addr2line=
for candidate in llvm-addr2line rust-llvm-addr2line addr2line; do
    if command -v "$candidate" >/dev/null 2>&1; then
        addr2line=$candidate
        break
    fi
done

if [ -n "$addr2line" ] && [ -n "$elf" ] && [ -r "$elf" ]; then
    awk '$2 == "kernel" { print $3 }' "$tmp/samples" | sort -u >"$tmp/pcs"
    if [ -s "$tmp/pcs" ]; then
        "$addr2line" -e "$elf" -f -C -p <"$tmp/pcs" >"$tmp/symbols"
        paste "$tmp/pcs" "$tmp/symbols" >"$tmp/map"
    else
        : >"$tmp/map"
    fi
    awk -F '\t' '
        NR == FNR { symbols[$1] = $2; next }
        {
            symbol = $2 == "kernel" ? symbols[$3] : "[user ELF not supplied]"
            if (symbol == "") symbol = "??"
            print $1 "\t" $2 "\t" $3 "\t" $4 "\t" symbol
        }
    ' "$tmp/map" "$tmp/samples" >"$tmp/resolved"
else
    awk -F '\t' '{ print $0 "\t[raw; pass kernel ELF as argument 2]" }' \
        "$tmp/samples" >"$tmp/resolved"
fi

printf 'case\tmode\tpc\tsamples\tshare%%\tsymbol\n'
awk -F '\t' '
    NR == FNR { total[$1] += $4; next }
    {
        share = total[$1] ? $4 * 100 / total[$1] : 0
        printf "%s\t%s\t%s\t%s\t%.2f\t%s\n", $1, $2, $3, $4, share, $5
    }
' "$tmp/resolved" "$tmp/resolved" | sort -t '	' -k1,1 -k4,4nr

echo
echo "TOP FUNCTIONS"
printf 'case\tsamples\tshare%%\tfunction\n'
awk -F '\t' '
    {
        function_name = $5
        sub(/ at .*/, "", function_name)
        key = $1 SUBSEP function_name
        samples[key] += $4
        total[$1] += $4
    }
    END {
        for (key in samples) {
            split(key, parts, SUBSEP)
            share = total[parts[1]] ? samples[key] * 100 / total[parts[1]] : 0
            printf "%s\t%.0f\t%.2f\t%s\n", parts[1], samples[key], share, parts[2]
        }
    }
' "$tmp/resolved" | sort -t '	' -k1,1 -k2,2nr
