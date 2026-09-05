#!/usr/bin/env bash
# Assemble, sign, notarize and zip Heed.app around a built `heed` binary.
#
#   scripts/package-macos-app.sh <heed-binary> <version> <target-triple> <out-dir>
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

# --- preconditions ---------------------------------------------------------

[ -f "$BINARY" ] || die "heed binary not found: $BINARY"
[ -x "$BINARY" ] || die "heed binary is not executable: $BINARY"
[ -n "$VERSION" ] || die "version must not be empty"
[ -n "$TRIPLE" ] || die "target triple must not be empty"

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

for tool in codesign ditto plutil shasum xattr; do
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

sed "s/__VERSION__/${VERSION}/g" "$RES_DIR/Info.plist" > "$APP/Contents/Info.plist"
grep -q '__VERSION__' "$APP/Contents/Info.plist" && die "version template not fully substituted"
cp "$RES_DIR/dev.heed.agent.plist" "$APP/Contents/Library/LaunchAgents/dev.heed.agent.plist"
cp "$RES_DIR/Heed.icns" "$APP/Contents/Resources/Heed.icns"
cp "$BINARY" "$APP/Contents/MacOS/heed"
chmod 755 "$APP/Contents/MacOS/heed"
plutil -lint "$APP/Contents/Info.plist" "$APP/Contents/Library/LaunchAgents/dev.heed.agent.plist"

# Finder metadata / resource forks on any file make codesign refuse the bundle.
xattr -cr "$APP"

if command -v otool >/dev/null 2>&1; then
    log "Deployment target of bundled binary:"
    otool -l "$APP/Contents/MacOS/heed" | grep -A4 LC_BUILD_VERSION | grep -E 'platform|minos|sdk' || true
fi

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
if ! codesign -d --verbose=4 "$APP" 2>&1 | grep -q 'Sealed Resources'; then
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
