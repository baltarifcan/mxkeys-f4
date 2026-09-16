#!/usr/bin/env bash
#
# Install a code-signed copy of the daemon at a stable path.
#
# Why this exists, and why it is not just a symlink into /nix/store:
#
# macOS keys TCC permissions — here Input Monitoring, to read the HID++ channel,
# and Accessibility, to post the key — to a binary's *designated requirement*.
# For an unsigned or ad-hoc-signed binary that requirement contains the cdhash of
# that exact build:
#
#   identifier "mxkeys-f4d" and cdhash H"<hash of this exact build>"
#
# Every rebuild changes the hash, so every upgrade would silently cost the daemon
# both grants. The failure mode is not an error dialog: the daemon starts, fails
# to open the device, and F4 goes back to typing F12. Signing with a certificate
# instead produces a requirement that is stable for as long as the certificate
# is:
#
#   identifier "mxkeys-f4d" and certificate leaf = H"<hash of the certificate>"
#
# The store path changes on every rebuild too, and TCC records the path, so the
# binary is copied to a fixed location rather than linked.
#
# The certificate is deliberately NOT added to the trust store. codesign signs
# perfectly well with an untrusted self-signed identity, the signature is just as
# stable, and skipping the trust step means this needs no sudo and raises no
# authorisation dialog — which is what makes it safe to run unattended from a
# Home Manager activation on a fresh machine.

set -euo pipefail

CN="${MXKEYS_SIGNING_CN:-mxkeys-f4 Local Signing}"
SRC="${1:?usage: install-signed.sh <source binary> <destination>}"
DEST="${2:?usage: install-signed.sh <source binary> <destination>}"

say() { printf 'mxkeys-f4: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }

# Deliberately not `find-identity -v`. The -v filter means "valid", which means
# "chains to a trusted root", which this certificate intentionally does not.
have_identity() {
  /usr/bin/security find-identity -p codesigning 2>/dev/null | grep -qF "\"$CN\""
}

create_identity() {
  command -v openssl >/dev/null || die "openssl not found"
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat > "$tmp/cfg" <<CFG
[ req ]
distinguished_name = dn
x509_extensions    = v3
prompt             = no
[ dn ]
CN = $CN
[ v3 ]
basicConstraints   = critical,CA:false
keyUsage           = critical,digitalSignature
extendedKeyUsage   = critical,codeSigning
CFG

  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cfg" 2>/dev/null \
    || die "openssl could not generate the certificate"

  # Two things Apple's Security framework is fussy about, both of which surface
  # as the same useless "MAC verification failed (wrong password?)" on import:
  # OpenSSL 3 defaults to a PBKDF2/AES MAC it cannot read, so the old SHA1/3DES
  # scheme has to be forced; and an *empty* passphrase fails MAC verification
  # outright, so the bundle gets a random one. It exists only between these two
  # lines. The private key's security comes from the login keychain, not from it.
  local pass
  pass="$(openssl rand -hex 16)"
  openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -out "$tmp/bundle.p12" -passout "pass:$pass" -name "$CN" \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 2>/dev/null \
    || die "openssl could not package the identity"

  /usr/bin/security import "$tmp/bundle.p12" \
    -k "$HOME/Library/Keychains/login.keychain-db" \
    -P "$pass" -T /usr/bin/codesign >/dev/null \
    || die "could not import the signing identity into the login keychain"

  say "created signing identity \"$CN\""
}

if ! have_identity; then
  create_identity
fi

mkdir -p "$(dirname "$DEST")"

# Skip when the installed copy already came from this exact source.
#
# Comparing $SRC to $DEST directly does not work -- $DEST carries a signature
# the source does not, so they never match and every activation would re-sign.
# Re-signing is not free: it mints a new cdhash, and anything that recorded the
# old one has to be re-approved. A sidecar recording the source store path is
# the cheap, correct test, since a store path already encodes its contents.
stamp="$DEST.source"
if [ -f "$DEST" ] && [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$SRC" ] \
   && /usr/bin/codesign --verify "$DEST" 2>/dev/null; then
  exit 0
fi

tmpbin="$DEST.new"
install -m 0755 "$SRC" "$tmpbin"
/usr/bin/codesign --force --sign "$CN" --identifier mxkeys-f4d --timestamp=none "$tmpbin" 2>/dev/null \
  || die "codesign failed"
mv -f "$tmpbin" "$DEST"
printf '%s' "$SRC" > "$stamp"
say "installed signed daemon at $DEST"
