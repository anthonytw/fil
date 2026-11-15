#!/usr/bin/env bash
# How old is the last full security audit? (SECURITY.md "Audits": a full audit every six months;
# a release warns, never stops, past seven.)
#
#   audit-age.sh [--dir DIR] [--today YYYY-MM-DD]
#
#   --dir    the reports (default: docs/audits of this repository)
#   --today  the date to measure from (default: today, local time)
#
# A report is named docs/audits/YYYY-MM-DD-KIND-VERSION.md, KIND one of full, release and
# full-release (docs/audits/README.md, "Publishing the report"); the date is the day the run was
# pinned. The last full audit is the newest `full` or `full-release` report. Other files (README.md,
# LANES.md, TEMPLATE.md) and names that do not parse are ignored.
#
# Prints one line. Exit 0 when the last full audit is at most seven months old; 3 (a warning, which
# the caller reports and goes on) when it is older or there is none; 2 on a usage error.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
dir=$here/../../docs/audits today=$(date +%Y-%m-%d)
while [ $# -gt 0 ]; do
  case $1 in
    --dir) dir=${2:?--dir needs a directory}; shift 2 ;;
    --today) today=${2:?--today needs a date}; shift 2 ;;
    *) echo "audit-age: unknown argument: $1" >&2; exit 2 ;;
  esac
done
date_re='^([0-9]{4})-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])$'
[[ $today =~ $date_re ]] || { echo "audit-age: --today $today is not YYYY-MM-DD" >&2; exit 2; }
ty=$((10#${BASH_REMATCH[1]})) tm=$((10#${BASH_REMATCH[2]})) td=$((10#${BASH_REMATCH[3]}))

last=''
if [ -d "$dir" ]; then
  for f in "$dir"/*.md; do
    [ -e "$f" ] || continue
    b=${f##*/}
    [[ $b =~ ^([0-9]{4}-[0-9]{2}-[0-9]{2})-(full|full-release)-[0-9A-Za-z][0-9A-Za-z.+~-]*\.md$ ]] || continue
    d=${BASH_REMATCH[1]}
    [[ $d =~ $date_re ]] || continue
    # ISO dates sort as strings
    if [ -z "$last" ] || [[ $d > ${last%% *} ]]; then last="$d $b"; fi
  done
fi
if [ -z "$last" ]; then
  echo "no full security audit yet in docs/audits (SECURITY.md \"Audits\": one every six months; the baseline full audit is due before 1.0)"
  exit 3
fi
d=${last%% *} name=${last#* }
[[ $d =~ $date_re ]]
ay=$((10#${BASH_REMATCH[1]})) am=$((10#${BASH_REMATCH[2]})) ad=$((10#${BASH_REMATCH[3]}))
months=$(((ty * 12 + tm) - (ay * 12 + am)))
if [ "$months" -gt 7 ] || { [ "$months" -eq 7 ] && [ "$td" -gt "$ad" ]; }; then
  echo "the last full security audit is $name, from $d: more than seven months ago (SECURITY.md \"Audits\": one every six months); run one (docs/audits/README.md)"
  exit 3
fi
echo "the last full security audit is $name, from $d (within seven months)"
