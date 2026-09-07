#!/usr/bin/env bash
# Assemble, sign, notarize and zip Heed.app around a built `heed` binary.
#
#   scripts/package-macos-app.sh <heed-binary> <version> <target-triple> <out-dir>
#
# <version> is MAJOR.MINOR.PATCH with an optional -prerelease suffix; a
# leading "v" (as in a git tag) is accepted and stripped. The full string
# becomes CFBundleShortVersionString and the zip name; CFBundleVersion is
# the numeric MAJOR.MINOR.PATCH only, as Apple requires.
#
# <heed-binary> must have been linked with MACOSX_DEPLOYMENT_TARGET=13.0
# (LC_BUILD_VERSION minos >= 13.0) — a `cargo build` inside this checkout
# does that via .cargo/config.toml; a `cargo install --git` build does not
# and is refused here, because SMAppService needs macOS 13.
#
# Writes:
#   <out-dir>/Heed.app
#   <out-dir>/Heed-<version>-<triple>.app.zip
#   <out-dir>/Heed-<version>-<triple>.app.zip.sha256
#
# Environment:
#   APPLE_SIGNING_IDENTITY   required. Developer ID Application identity in the
#                            keychain. The script refuses to run without it —
#                            an unsigned bundle is never emitted.
#   APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
#                            when all three are set the bundle is notarized
#                            (`xcrun notarytool submit --wait`) and stapled.
#                            When any is unset notarization is skipped and the
#                            script says so — this is what makes it runnable on
#                            a developer Mac with only the signing identity.
#   SKIP_NOTARIZE=1          skip notarization even if the credentials are set.
#
# The bundle layout is the contract the heed binary itself relies on
# (src/service/bundle.rs): Contents/MacOS/heed with
# Contents/Library/LaunchAgents/dev.heed.agent.plist beside it. Do not rename
# either without changing the binary.

set -euo pipefail

usage() {
    echo "usage: $0 <heed-binary> <version> <target-triple> <out-dir>" >&2
    exit 2
}

[ $# -eq 4 ] || usage
BINARY="$1"
VERSION="$2"
TRIPLE="$3"
OUT_DIR="$4"

log() { printf '==> %s\n' "$*"; }
die() { printf 'package-macos-app: error: %s\n' "$*" >&2; exit 1; }

# Minimum LC_BUILD_VERSION minos the bundled binary must carry (SMAppService).
MIN_MINOS_MAJOR=13
MIN_MINOS_MINOR=0

# --- preconditions ---------------------------------------------------------

[ -f "$BINARY" ] || die "heed binary not found: $BINARY"
[ -x "$BINARY" ] || die "heed binary is not executable: $BINARY"
[ -n "$VERSION" ] || die "version must not be empty"
[ -n "$TRIPLE" ] || die "target triple must not be empty"

# Version shape: MAJOR.MINOR.PATCH[-prerelease], optional leading "v".
VERSION="${VERSION#v}"
if ! [[ "$VERSION" =~ ^([0-9]+\.[0-9]+\.[0-9]+)(-[0-9A-Za-z.-]+)?$ ]]; then
    die "invalid version '$2': expected MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-prerelease (a leading 'v' is allowed), e.g. 0.3.1 or v0.3.1-rc.1"
fi
BUNDLE_VERSION="${BASH_REMATCH[1]}"

if [ -z "${APPLE_SIGNING_IDENTITY:-}" ]; then
    die "APPLE_SIGNING_IDENTITY is not set. Refusing to build an unsigned Heed.app — SMAppService rejects unsealed bundles. Set APPLE_SIGNING_IDENTITY to a 'Developer ID Application: …' identity present in the keychain."
fi

NOTARIZE=1
if [ "${SKIP_NOTARIZE:-0}" = "1" ]; then
    NOTARIZE=0
    log "SKIP_NOTARIZE=1 — notarization skipped by request"
else
    MISSING=()
    [ -n "${APPLE_ID:-}" ] || MISSING+=(APPLE_ID)
    [ -n "${APPLE_PASSWORD:-}" ] || MISSING+=(APPLE_PASSWORD)
    [ -n "${APPLE_TEAM_ID:-}" ] || MISSING+=(APPLE_TEAM_ID)
    if [ ${#MISSING[@]} -gt 0 ]; then
        NOTARIZE=0
        log "Notarization skipped: ${MISSING[*]} not set. The bundle will be signed but NOT notarized; Gatekeeper (spctl) will reject it. Set APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID to notarize."
    fi
fi

for tool in codesign ditto otool plutil shasum xattr; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done
if [ "$NOTARIZE" = 1 ]; then
    xcrun --find notarytool >/dev/null 2>&1 || die "xcrun notarytool not available (Xcode command line tools required)"
    xcrun --find stapler >/dev/null 2>&1 || die "xcrun stapler not available (Xcode command line tools required)"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RES_DIR="$SCRIPT_DIR/../resources/macos"
for f in Info.plist dev.heed.agent.plist Heed.icns; do
    [ -f "$RES_DIR/$f" ] || die "missing bundle resource: $RES_DIR/$f"
done

# --- assemble --------------------------------------------------------------

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
APP="$OUT_DIR/Heed.app"
ZIP_NAME="Heed-${VERSION}-${TRIPLE}.app.zip"
ZIP="$OUT_DIR/$ZIP_NAME"

log "Assembling $APP (version $VERSION, $TRIPLE)"
rm -rf "$APP" "$ZIP" "$ZIP.sha256"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Library/LaunchAgents"

sed -e "s/__VERSION__/${VERSION}/g" -e "s/__BUNDLE_VERSION__/${BUNDLE_VERSION}/g" \
    "$RES_DIR/Info.plist" > "$APP/Contents/Info.plist"
grep -q '__' "$APP/Contents/Info.plist" && die "version template not fully substituted"
cp "$RES_DIR/dev.heed.agent.plist" "$APP/Contents/Library/LaunchAgents/dev.heed.agent.plist"
cp "$RES_DIR/Heed.icns" "$APP/Contents/Resources/Heed.icns"
cp "$BINARY" "$APP/Contents/MacOS/heed"
chmod 755 "$APP/Contents/MacOS/heed"
plutil -lint "$APP/Contents/Info.plist" "$APP/Contents/Library/LaunchAgents/dev.heed.agent.plist"

# Finder metadata / resource forks on any file make codesign refuse the bundle.
xattr -cr "$APP"

# Refuse a binary whose deployment target is below macOS 13: SMAppService is
# unavailable there, and a `cargo install --git` build (outside this
# checkout, so without .cargo/config.toml's MACOSX_DEPLOYMENT_TARGET) would
# silently carry the SDK default.
LOAD_CMDS="$(otool -l "$APP/Contents/MacOS/heed")"
log "Deployment target of bundled binary:"
printf '%s\n' "$LOAD_CMDS" | grep -A4 LC_BUILD_VERSION | grep -E 'platform|minos|sdk' || true
MINOS="$(printf '%s\n' "$LOAD_CMDS" | awk '
    /LC_BUILD_VERSION/ { in_build = 1; next }
    in_build && $1 == "minos" { print $2; exit }
    /^Load command/ { in_build = 0 }
')"
if [ -z "$MINOS" ]; then
    # Older toolchains write LC_VERSION_MIN_MACOSX instead.
    MINOS="$(printf '%s\n' "$LOAD_CMDS" | awk '
        /LC_VERSION_MIN_MACOSX/ { in_min = 1; next }
        in_min && $1 == "version" { print $2; exit }
        /^Load command/ { in_min = 0 }
    ')"
fi
[ -n "$MINOS" ] || die "could not read the deployment target (LC_BUILD_VERSION minos) of $BINARY"
MINOS_MAJOR="${MINOS%%.*}"
MINOS_REST="${MINOS#*.}"
MINOS_MINOR="${MINOS_REST%%.*}"
[ "$MINOS_REST" = "$MINOS" ] && MINOS_MINOR=0
if ! [[ "$MINOS_MAJOR" =~ ^[0-9]+$ && "$MINOS_MINOR" =~ ^[0-9]+$ ]]; then
    die "unparseable deployment target '$MINOS' in $BINARY"
fi
if [ "$MINOS_MAJOR" -lt "$MIN_MINOS_MAJOR" ] || { [ "$MINOS_MAJOR" -eq "$MIN_MINOS_MAJOR" ] && [ "$MINOS_MINOR" -lt "$MIN_MINOS_MINOR" ]; }; then
    die "$BINARY was linked for macOS $MINOS; Heed.app needs a deployment target of at least $MIN_MINOS_MAJOR.$MIN_MINOS_MINOR (SMAppService). Build it inside this checkout (cargo build --release picks up MACOSX_DEPLOYMENT_TARGET=13.0 from .cargo/config.toml), not with cargo install."
fi
log "Deployment target $MINOS >= $MIN_MINOS_MAJOR.$MIN_MINOS_MINOR: ok"

# --- sign ------------------------------------------------------------------

log "Signing Contents/MacOS/heed with '$APPLE_SIGNING_IDENTITY'"
codesign --force --options runtime --timestamp \
    --sign "$APPLE_SIGNING_IDENTITY" "$APP/Contents/MacOS/heed"

log "Signing bundle"
codesign --force --options runtime --timestamp \
    --sign "$APPLE_SIGNING_IDENTITY" "$APP"

log "codesign --verify --deep --strict --verbose=2"
codesign --verify --deep --strict --verbose=2 "$APP"

# The agent plist must be inside the seal or SMAppService will not load it.
# Capture first: `codesign | grep -q` races under pipefail (grep exits on the
# match, codesign gets SIGPIPE, the pipeline reports failure ~1 run in 3).
SIG_INFO="$(codesign -d --verbose=4 "$APP" 2>&1)"
if ! printf '%s\n' "$SIG_INFO" | grep -q 'Sealed Resources'; then
    die "bundle signature has no sealed resources"
fi
if ! grep -q 'Library/LaunchAgents/dev.heed.agent.plist' "$APP/Contents/_CodeSignature/CodeResources"; then
    die "dev.heed.agent.plist is not in the sealed resources"
fi
log "Sealed: Contents/Library/LaunchAgents/dev.heed.agent.plist"

# --- notarize + staple -----------------------------------------------------

if [ "$NOTARIZE" = 1 ]; then
    NOTARY_ZIP="$(mktemp -d)/Heed-notarize.zip"
    log "Submitting for notarization (team $APPLE_TEAM_ID)"
    ditto -c -k --keepParent "$APP" "$NOTARY_ZIP"
    xcrun notarytool submit "$NOTARY_ZIP" \
        --apple-id "$APPLE_ID" \
        --password "$APPLE_PASSWORD" \
        --team-id "$APPLE_TEAM_ID" \
        --wait
    rm -f "$NOTARY_ZIP"
    log "Stapling notarization ticket"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
fi

# --- Gatekeeper assessment -------------------------------------------------

log "spctl -a -vv"
set +e
SPCTL_OUT="$(spctl -a -vv "$APP" 2>&1)"
SPCTL_RC=$?
set -e
printf '%s\n' "$SPCTL_OUT"
if [ "$NOTARIZE" = 1 ]; then
    [ "$SPCTL_RC" -eq 0 ] || die "Gatekeeper rejected the notarized bundle (spctl exit $SPCTL_RC)"
    printf '%s\n' "$SPCTL_OUT" | grep -q 'Notarized Developer ID' || die "spctl did not report source=Notarized Developer ID"
else
    log "spctl exit $SPCTL_RC (rejection is expected: notarization was skipped)"
fi

# --- zip + checksum --------------------------------------------------------

log "Writing $ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
(cd "$OUT_DIR" && shasum -a 256 "$ZIP_NAME" > "$ZIP_NAME.sha256")
(cd "$OUT_DIR" && shasum -a 256 -c "$ZIP_NAME.sha256")

log "Done"
printf '  %s\n' "$APP" "$ZIP" "$ZIP.sha256"
if [ "$NOTARIZE" = 1 ]; then
    log "Bundle is signed, notarized and stapled"
else
    log "Bundle is signed but NOT notarized"
fi
