#!/usr/bin/env bash
# Prints CARTEIRO_UPDATE_PUBKEY=<base64 raw Ed25519 public key> for
# $GITHUB_ENV, derived from the UPDATE_SIGNING_KEY secret (PEM). Prints
# nothing without the secret: that build simply has no update channel.
set -euo pipefail
[ -n "${UPDATE_SIGNING_KEY:-}" ] || { echo "UPDATE_SIGNING_KEY not set: building without auto-update" >&2; exit 0; }
# The DER public key ends with the 32 raw key bytes.
pub=$(printf '%s\n' "$UPDATE_SIGNING_KEY" | openssl pkey -pubout -outform DER | tail -c 32 | base64 | tr -d '\r\n')
echo "CARTEIRO_UPDATE_PUBKEY=$pub"
