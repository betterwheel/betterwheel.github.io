#!/usr/bin/env bash
# Sign Windows binaries with Azure Trusted Signing via jsign.
# Usage:
#   scripts/sign-windows.sh file1.exe [file2.exe ...]
#
# Prereqs (one-time, see CLAUDE.md "Code signing (Windows)"):
#   - Azure Trusted Signing account + Public Trust cert profile (approved)
#   - jsign, azure-cli installed (osslsigncode optional, used for verify)
#   - `az login` performed
#   - Keychain item "betterwheel-signing" populated with endpoint/account/profile

set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "usage: $0 file1.exe [file2.exe ...]" >&2
  exit 2
fi

for tool in jsign az; do
  if ! command -v "${tool}" >/dev/null 2>&1; then
    echo "error: ${tool} not on PATH. see CLAUDE.md 'Code signing (Windows)' for install." >&2
    exit 1
  fi
done

CONFIG_JSON="$(security find-generic-password -s betterwheel-signing -w 2>/dev/null || true)"
if [ -z "${CONFIG_JSON}" ]; then
  echo "error: keychain item 'betterwheel-signing' not found. see CLAUDE.md." >&2
  exit 1
fi

ENDPOINT="$(printf '%s' "${CONFIG_JSON}" | /usr/bin/python3 -c 'import json,sys;print(json.load(sys.stdin)["endpoint"])')"
ACCOUNT="$(printf  '%s' "${CONFIG_JSON}" | /usr/bin/python3 -c 'import json,sys;print(json.load(sys.stdin)["account"])')"
PROFILE="$(printf  '%s' "${CONFIG_JSON}" | /usr/bin/python3 -c 'import json,sys;print(json.load(sys.stdin)["profile"])')"

# Trusted Signing access tokens expire in ~1h; grab a fresh one per run
# rather than caching.
TOKEN="$(az account get-access-token --resource https://codesigning.azure.net --query accessToken -o tsv 2>/dev/null || true)"
if [ -z "${TOKEN}" ]; then
  echo "error: az account get-access-token failed. run 'az login' first." >&2
  exit 1
fi

for f in "$@"; do
  if [ ! -f "${f}" ]; then
    echo "skip: ${f} not a file" >&2
    continue
  fi
  echo ">> signing ${f}"
  jsign \
    --storetype TRUSTEDSIGNING \
    --keystore "${ENDPOINT}" \
    --storepass "${TOKEN}" \
    --alias "${ACCOUNT}/${PROFILE}" \
    --tsaurl http://timestamp.acs.microsoft.com \
    --tsmode RFC3161 \
    --name "BetterWheel" \
    --url "https://github.com/betterwheel/betterwheel" \
    --replace \
    "${f}"
  if command -v osslsigncode >/dev/null 2>&1; then
    echo ">> verifying ${f}"
    osslsigncode verify -in "${f}" >/dev/null
  fi
done

echo "done."
