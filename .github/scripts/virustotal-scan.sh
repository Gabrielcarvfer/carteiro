#!/usr/bin/env bash
# Uploads each file to VirusTotal and fails if any engine flags it as
# malicious or suspicious. Needs VIRUSTOTAL_API_KEY. Uses only curl and jq.
#
# Usage: virustotal-scan.sh FILE...
set -euo pipefail

: "${VIRUSTOTAL_API_KEY:?set the VIRUSTOTAL_API_KEY repository secret}"
[ "$#" -gt 0 ] || { echo "no files to scan" >&2; exit 1; }
API=https://www.virustotal.com/api/v3
POLL=30          # seconds; the free API allows 4 requests a minute
TIMEOUT=2400     # seconds per file

vt() { curl -fsS --proto '=https' --retry 3 --retry-delay 20 -H "x-apikey: $VIRUSTOTAL_API_KEY" "$@"; }

failed=0
for f in "$@"; do
  name=$(basename "$f")
  size=$(stat -c %s "$f")
  sha=$(sha256sum "$f" | cut -d' ' -f1)
  echo "::group::$name ($size bytes, sha256 $sha)"
  # Files over 32 MB go to a one-time upload URL.
  if [ "$size" -gt $((32 * 1024 * 1024)) ]; then
    url=$(vt "$API/files/upload_url" | jq -r .data)
  else
    url="$API/files"
  fi
  analysis=$(vt -X POST -F "file=@$f" "$url" | jq -r .data.id)
  [ -n "$analysis" ] && [ "$analysis" != null ] || { echo "upload failed" >&2; exit 1; }

  waited=0
  while :; do
    sleep "$POLL"; waited=$((waited + POLL))
    result=$(vt "$API/analyses/$analysis")
    [ "$(jq -r .data.attributes.status <<<"$result")" = completed ] && break
    if [ "$waited" -ge "$TIMEOUT" ]; then
      echo "::error::$name: VirusTotal didn't finish within $((TIMEOUT / 60)) minutes"; exit 1
    fi
  done

  read -r malicious suspicious harmless undetected < <(jq -r '.data.attributes.stats | "\(.malicious) \(.suspicious) \(.harmless) \(.undetected)"' <<<"$result")
  echo "malicious=$malicious suspicious=$suspicious harmless=$harmless undetected=$undetected"
  echo "Report: https://www.virustotal.com/gui/file/$sha"
  if [ "$malicious" -gt 0 ] || [ "$suspicious" -gt 0 ]; then
    jq -r '.data.attributes.results[] | select(.category == "malicious" or .category == "suspicious") | "  \(.engine_name): \(.result)"' <<<"$result"
    echo "::error::$name: flagged by $malicious engine(s) as malicious and $suspicious as suspicious"
    failed=1
  fi
  {
    echo "| \`$name\` | $malicious | $suspicious | [report](https://www.virustotal.com/gui/file/$sha) |"
  } >> "${VT_SUMMARY:-/dev/null}"
  echo "::endgroup::"
done
exit "$failed"
