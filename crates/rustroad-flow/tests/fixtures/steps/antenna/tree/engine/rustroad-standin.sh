#!/bin/sh
# usage: <this> --workspace W --threads N --max-expansions E repair-antenna-groups --base-rev R --plan P
set -eu
here=$(cd "$(dirname "$0")" && pwd -P)
root=$(dirname "$here")
workspace=$2
plan=${11}
out=$(dirname "$plan")/native-reports
mkdir "$out"
total=0
for file in "$here"/reports/*.json; do
  name=$(basename "$file")
  sed "s|@ROOT@|$root|g" "$file" > "$out/$name"
  total=$((total + $(wc -c < "$out/$name")))
done
sed "s|@REPORT_BYTES@|$total|" "$here/summary.json" > "$out/summary.json"
mkdir "$workspace/revisions/rev6"
cp "$here/rev6-design.json" "$workspace/revisions/rev6/design.json"
printf 'rev6\n' > "$workspace/HEAD.next"
mv "$workspace/HEAD.next" "$workspace/HEAD"
cat "$out/summary.json"
