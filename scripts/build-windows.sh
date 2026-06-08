#!/usr/bin/env bash
# Cross-compile betterwheel from macOS to Windows (x86_64-pc-windows-msvc)
# using cargo-xwin. Produces a single self-contained .exe; the frontend
# assets in dist/ and the icon are embedded by tauri-build at compile time.
#
# Prereqs (one-time):
#   ~/.cargo/bin/rustup target add x86_64-pc-windows-msvc
#   ~/.cargo/bin/rustup component add llvm-tools-preview
#   ~/.cargo/bin/cargo install --locked cargo-xwin
#
# On first run, cargo-xwin will download ~700 MB of MSVC headers/CRT into
# ~/.cache/cargo-xwin/ — subsequent runs reuse the cache. Initial Windows
# build then takes 5–15 minutes; incremental builds are much faster.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Usage: build-windows.sh [release|debug] [x64|arm64|all]
#   default profile: release
#   default arch:    x64
PROFILE="${1:-release}"
ARCH="${2:-x64}"

case "${ARCH}" in
  x64)   TARGETS=("x86_64-pc-windows-msvc") ;;
  arm64) TARGETS=("aarch64-pc-windows-msvc") ;;
  all)   TARGETS=("x86_64-pc-windows-msvc" "aarch64-pc-windows-msvc") ;;
  *) echo "error: unknown arch '${ARCH}' (expected x64, arm64, or all)" >&2; exit 1 ;;
esac

# This box has both MacPorts Rust (in /opt/local/bin) and rustup-managed Rust
# (in ~/.cargo/bin + ~/.rustup/toolchains/...). Only rustup knows about
# cross-targets, so force its toolchain ahead of MacPorts in PATH for every
# subprocess that cargo-xwin spawns.
RUSTUP_TOOLCHAIN_BIN="${HOME}/.rustup/toolchains/stable-aarch64-apple-darwin/bin"
export PATH="${HOME}/.cargo/bin:${RUSTUP_TOOLCHAIN_BIN}:${PATH}"
CARGO="${HOME}/.cargo/bin/cargo"

# tauri-winres needs llvm-rc to embed the icon + version info into the .exe.
# rustup's llvm-tools-preview component doesn't ship llvm-rc, so probe other
# common locations and append the first match to PATH.
find_llvm_rc() {
  if command -v llvm-rc >/dev/null 2>&1; then
    command -v llvm-rc
    return
  fi
  local c
  for c in "${HOME}"/Library/Android/sdk/ndk/*/toolchains/llvm/prebuilt/*/bin/llvm-rc \
           /opt/local/libexec/llvm-*/bin/llvm-rc \
           /opt/homebrew/opt/llvm/bin/llvm-rc \
           /usr/local/opt/llvm/bin/llvm-rc; do
    if [ -x "${c}" ]; then echo "${c}"; return; fi
  done
}
LLVM_RC="$(find_llvm_rc || true)"
if [ -z "${LLVM_RC}" ]; then
  echo "error: llvm-rc not found. install one of:" >&2
  echo "       brew install llvm" >&2
  echo "       sudo port install llvm-22" >&2
  echo "       (or any Android NDK install includes it)" >&2
  exit 1
fi
# Append to PATH (after rustup tools so we don't accidentally pick up the
# NDK's clang for normal cross-compile — we just need its llvm-rc).
export PATH="${PATH}:$(dirname "${LLVM_RC}")"
echo "using llvm-rc: ${LLVM_RC}"

if [ ! -x "${CARGO}" ]; then
  echo "error: rustup-managed cargo not found at ${CARGO}" >&2
  echo "       install rustup from https://rustup.rs/" >&2
  exit 1
fi

if ! "${CARGO}" xwin --help >/dev/null 2>&1; then
  echo "error: cargo-xwin not installed. run:" >&2
  echo "       ${CARGO} install --locked cargo-xwin" >&2
  exit 1
fi

INSTALLED_TARGETS="$("${HOME}/.cargo/bin/rustup" target list --installed 2>/dev/null || true)"
for t in "${TARGETS[@]}"; do
  if ! echo "${INSTALLED_TARGETS}" | grep -q "^${t}$"; then
    echo "error: ${t} not installed. run:" >&2
    echo "       ${HOME}/.cargo/bin/rustup target add ${t}" >&2
    exit 1
  fi
done

cd "${PROJECT_ROOT}/src-tauri"

if [ "${PROFILE}" = "debug" ]; then
  BUILD_FLAGS=""
  PROFILE_DIR="debug"
elif [ "${PROFILE}" = "release" ]; then
  BUILD_FLAGS="--release"
  PROFILE_DIR="release"
else
  echo "error: unknown profile '${PROFILE}' (expected release or debug)" >&2; exit 1
fi

echo ">> cross-compiling betterwheel (profile: ${PROFILE}, targets: ${TARGETS[*]})"
echo ">> first run pulls MSVC headers via cargo-xwin (~700 MB) — be patient"
echo

declare -a BUILT_PATHS=()
for t in "${TARGETS[@]}"; do
  echo "---- ${t} ----"
  "${CARGO}" xwin build ${BUILD_FLAGS} --target "${t}"
  OUT="${PROJECT_ROOT}/src-tauri/target/${t}/${PROFILE_DIR}/betterwheel-desktop.exe"
  if [ ! -f "${OUT}" ]; then
    echo "error: expected output not found at ${OUT}" >&2
    exit 1
  fi
  BUILT_PATHS+=("${OUT}")
done

echo
echo "================================================================"
for p in "${BUILT_PATHS[@]}"; do
  echo " built: ${p}  ($(du -h "${p}" | cut -f1))"
done
echo "================================================================"

# --- Optional: wrap the x64 .exe in an NSIS installer ---
# Triggered when env BUILD_INSTALLER=1 (set by user) and makensis is on PATH.
if [ "${BUILD_INSTALLER:-0}" = "1" ]; then
  if ! command -v makensis >/dev/null 2>&1; then
    echo
    echo "warning: BUILD_INSTALLER=1 but makensis not found. install NSIS:"
    echo "         sudo port install nsis    (MacPorts)"
    echo "         brew install makensis     (Homebrew)"
    exit 1
  fi

  X64_EXE="${PROJECT_ROOT}/src-tauri/target/x86_64-pc-windows-msvc/${PROFILE_DIR}/betterwheel-desktop.exe"
  if [ ! -f "${X64_EXE}" ]; then
    echo "warning: x64 .exe not in this build (need it for the installer). re-run with 'all' or 'x64'." >&2
    exit 1
  fi

  WV2_BOOTSTRAPPER="${HOME}/.cache/betterwheel/MicrosoftEdgeWebview2Setup.exe"
  if [ ! -f "${WV2_BOOTSTRAPPER}" ]; then
    echo
    echo ">> downloading WebView2 bootstrapper from Microsoft (one-time, cached at ${WV2_BOOTSTRAPPER})"
    mkdir -p "$(dirname "${WV2_BOOTSTRAPPER}")"
    curl -fsSL -o "${WV2_BOOTSTRAPPER}" \
      "https://go.microsoft.com/fwlink/p/?LinkId=2124703"
  fi

  # Verify the bootstrapper is a valid Authenticode binary signed by Microsoft
  # before bundling it into our own (about-to-be-signed) installer. A poisoned
  # download cache — or a one-time TLS/DNS MITM at download time — would
  # otherwise be baked into every installer we ship, under our signature.
  # Runs on every installer build (not just on download) so a tampered cache is
  # always caught. Set WV2_SKIP_VERIFY=1 to bypass (not recommended).
  if [ "${WV2_SKIP_VERIFY:-0}" != "1" ]; then
    if ! command -v osslsigncode >/dev/null 2>&1; then
      echo "error: osslsigncode not found — cannot verify the WebView2 bootstrapper." >&2
      echo "       install it:  sudo port install osslsigncode   (or: brew install osslsigncode)" >&2
      echo "       or bypass (not recommended):  WV2_SKIP_VERIFY=1 BUILD_INSTALLER=1 $0 ..." >&2
      exit 1
    fi
    echo ">> verifying WebView2 bootstrapper Authenticode signature"
    # Chain validation needs Microsoft's code-signing roots, which are NOT in
    # the default OpenSSL CA store (they're not TLS roots) — use the bundle
    # pinned in-repo (scripts/microsoft-roots.pem; 2010 + 2011 + Identity
    # Verification 2020). The bootstrapper carries TWO signatures:
    #   Index 0 (primary)  — real Microsoft Corporation signature; must chain.
    #   Index 1 ("EdgeBuild") — self-signed tag signature holding the
    #     appguid/appname install parameters; by design it never chains to a
    #     public root, so it is excluded from the chain requirement.
    MS_ROOTS="${PROJECT_ROOT}/scripts/microsoft-roots.pem"
    WV2_VERIFY="$(osslsigncode verify -CAfile "${MS_ROOTS}" -TSA-CAfile "${MS_ROOTS}" \
      "${WV2_BOOTSTRAPPER}" 2>&1 || true)"
    WV2_PRIMARY="$(sed -n '/^Signature Index: 0/,/^Signature Index: [1-9]/p' <<<"${WV2_VERIFY}")"
    if ! grep -q "Signature verification: ok" <<<"${WV2_PRIMARY}"; then
      echo "error: WebView2 bootstrapper primary signature failed verification." >&2
      echo "       delete ${WV2_BOOTSTRAPPER} and re-run to re-download." >&2
      echo "${WV2_VERIFY}" >&2
      exit 1
    fi
    if ! grep -qi "Subject: CN=Microsoft Corporation" <<<"${WV2_PRIMARY}"; then
      echo "error: WebView2 bootstrapper is not signed by Microsoft Corporation." >&2
      echo "       delete ${WV2_BOOTSTRAPPER} and re-run to re-download." >&2
      echo "${WV2_VERIFY}" >&2
      exit 1
    fi
    echo "   bootstrapper signature OK (Microsoft Corporation, chain verified)"
  fi

  APP_VERSION="$(node -e "console.log(require('${PROJECT_ROOT}/package.json').version)")"
  OUT_FILE="${PROJECT_ROOT}/src-tauri/target/betterwheel-setup-${APP_VERSION}.exe"

  echo
  echo ">> compiling NSIS installer (version ${APP_VERSION})"
  makensis -V2 \
    "-DAPP_VERSION=${APP_VERSION}" \
    "-DAPP_EXE_PATH=${X64_EXE}" \
    "-DWEBVIEW2_BOOTSTRAPPER=${WV2_BOOTSTRAPPER}" \
    "-DICON_PATH=${PROJECT_ROOT}/src-tauri/icons/icon.ico" \
    "-DOUT_FILE=${OUT_FILE}" \
    "${PROJECT_ROOT}/scripts/installer.nsi"

  echo
  echo " installer: ${OUT_FILE}  ($(du -h "${OUT_FILE}" | cut -f1))"
  echo "================================================================"
fi

echo
echo "Each .exe is self-contained (frontend + icon embedded)."
echo "For an NSIS installer that bundles a WebView2 bootstrapper, re-run with"
echo "  BUILD_INSTALLER=1 scripts/build-windows.sh release x64"
