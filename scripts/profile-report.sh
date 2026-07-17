#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <serial-log>" >&2
    exit 2
fi

log=$1
tr -d '\r' <"$log" | awk '
function value(name,    i, pair) {
    for (i = 1; i <= NF; i++) {
        split($i, pair, "=")
        if (pair[1] == name) return pair[2]
    }
    return ""
}
/^@@PROFILE_STATS_BEGIN / {
    phase = value("phase")
    case_id = value("case")
    active = 1
    next
}
/^@@PROFILE_STATS_END / {
    active = 0
    next
}
active && /^cpu=/ {
    event = value("event")
    key = case_id SUBSEP event SUBSEP phase
    calls[key] += value("calls")
    cycles[key] += value("cycles")
    bytes[key] += value("bytes")
    packets[key] += value("packets")
    observed[key] = 1
    next
}
END {
    print "case\tevent\tcalls\tcycles\tbytes\tpackets\tcycles/call\tcycles/byte\tcycles/packet"
    for (key in observed) {
        split(key, parts, SUBSEP)
        if (parts[3] != "after") continue
        before = parts[1] SUBSEP parts[2] SUBSEP "before"
        dcalls = calls[key] - calls[before]
        dcycles = cycles[key] - cycles[before]
        dbytes = bytes[key] - bytes[before]
        dpackets = packets[key] - packets[before]
        per_call = dcalls ? dcycles / dcalls : 0
        per_byte = dbytes ? dcycles / dbytes : 0
        per_packet = dpackets ? dcycles / dpackets : 0
        printf "%s\t%s\t%.0f\t%.0f\t%.0f\t%.0f\t%.2f\t%.4f\t%.2f\n", \
            parts[1], parts[2], dcalls, dcycles, dbytes, dpackets, \
            per_call, per_byte, per_packet
    }
}
' | {
    IFS= read -r header
    printf '%s\n' "$header"
    sort
}
