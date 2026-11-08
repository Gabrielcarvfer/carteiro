#!/bin/sh
# Captures all mail traffic while logging in with a FAKE password over IMAP,
# SMTP and POP3 (TLS and STARTTLS), then searches the capture for that
# password in clear text and base64. Run inside the toolbox (needs dumpcap):
#   scripts/wire-check.sh [imap-host]
# Logins always use a made-up user name, never a real account.
set -eu
cd "$(dirname "$0")/.."
FAKE="Carteiro-Wire-Test-7f3a9c"
PCAP="$(mktemp -d)/wire.pcapng"
cargo build -q -p mail-core --example wire_probe
dumpcap -q -i any -f "tcp port 25 or tcp port 110 or tcp port 143 or tcp port 465 or tcp port 587 or tcp port 993 or tcp port 995" -w "$PCAP" &
CAP=$!
sleep 2
./target/debug/examples/wire_probe "$@" || true
sleep 2
kill "$CAP"; wait "$CAP" 2>/dev/null || true

echo
echo "Capture: $PCAP ($(tshark -r "$PCAP" 2>/dev/null | wc -l) packets)"
b64() { printf '%s' "$1" | base64 -w0; }
USER="carteiro-wire-test@example.invalid"
FOUND=0
for needle in "$FAKE" "$(b64 "$FAKE")" "$(b64 "$(printf '\0%s\0%s' "$USER" "$FAKE")")" "$(b64 "$USER")"; do
  n=$(tshark -r "$PCAP" -Y "frame contains \"$needle\"" 2>/dev/null | wc -l)
  echo "  frames containing '$needle': $n"
  [ "$n" -eq 0 ] || FOUND=1
done
echo "  plaintext payload frames after STARTTLS/implicit TLS ports (non-TLS data on 993/465/995):"
tshark -r "$PCAP" -Y "tcp.len>0 && (tcp.port==993 || tcp.port==465 || tcp.port==995) && !tls" 2>/dev/null | wc -l
if [ "$FOUND" -eq 0 ]; then echo "PASS: credentials never appeared on the wire."; else echo "FAIL: credentials visible in capture!"; exit 1; fi
