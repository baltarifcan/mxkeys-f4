#!/usr/bin/env bash
# Build the daemon, install a signed copy at a stable path, and load the two
# launchd agents behind it.
#
#   ./install.sh              build, install, load
#   ./install.sh --uninstall  unload and remove
#
# Needs a Rust toolchain. `mise use -g rust` is enough; so is rustup.
#
# The signing identity is what makes a rebuild safe: TCC pins Input Monitoring
# and Accessibility to the certificate, not to the build, so re-running this
# does not cost either grant. See scripts/install-signed.sh for the reasoning.

set -euo pipefail

# The universal MX Keys. Not "MX Keys for Mac", which is a different product id.
VENDOR=1133      # 0x046d, Logitech
PRODUCT=45915    # 0xb35b
CONTROL=225      # 0x00e1, the control in the F4 position
KEYCODE=79       # F18: no Apple or Logitech keyboard has one, so it costs nothing

# macOS symbolic hotkey to bind to that key code. 160 is "Show Apps" on macOS
# 26+ and "Show Launchpad" before it. Set to empty to bind the key yourself --
# the daemon only emits F18, it does not care what macOS does with it.
SYMBOLIC_HOTKEY=160

DEST="$HOME/.local/libexec/mxkeys-f4d"
AGENTS="$HOME/Library/LaunchAgents"
LOG="$HOME/Library/Logs/mxkeys-f4.log"
LABEL=local.mxkeys-f4
WATCHDOG_LABEL=local.mxkeys-f4-watchdog
WATCHDOG="$HOME/.local/bin/mxkeys-f4-watchdog"

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
U=$(id -u)

say()  { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
note() { printf '    %s\n' "$1"; }
die()  { printf '\n\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

if [ "${1:-}" = "--uninstall" ]; then
  say "Removing mxkeys-f4"
  launchctl bootout "gui/$U/$WATCHDOG_LABEL" 2>/dev/null || true
  launchctl bootout "gui/$U/$LABEL" 2>/dev/null || true
  rm -f "$AGENTS/$LABEL.plist" "$AGENTS/$WATCHDOG_LABEL.plist" "$DEST" "$DEST.source"
  note "The signing identity is left in the login keychain. Remove it from"
  note "Keychain Access if you want the TCC grants invalidated too."
  exit 0
fi

command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust first:
    mise use -g rust"

say "Building"
cargo build --release --manifest-path "$here/Cargo.toml"
BIN="$here/target/release/mxkeys-f4d"
[ -x "$BIN" ] || die "the build produced no $BIN"

say "Installing a signed copy at $DEST"
"$here/scripts/install-signed.sh" "$BIN" "$DEST"

if [ -n "$SYMBOLIC_HOTKEY" ]; then
  say "Binding symbolic hotkey $SYMBOLIC_HOTKEY to key code $KEYCODE"
  # 65535 is "no character" -- the binding is by key code, not by the character
  # the key would type -- and the trailing 0 is the modifier mask, i.e. none.
  defaults write com.apple.symbolichotkeys AppleSymbolicHotKeys -dict-add \
    "$SYMBOLIC_HOTKEY" "<dict>
      <key>enabled</key><true/>
      <key>value</key><dict>
        <key>parameters</key><array>
          <integer>65535</integer>
          <integer>$KEYCODE</integer>
          <integer>0</integer>
        </array>
        <key>type</key><string>standard</string>
      </dict>
    </dict>"

  # Without this the binding only takes effect at the next login: the hotkey
  # table is read once by the WindowServer session.
  act=/System/Library/PrivateFrameworks/SystemAdministration.framework/Resources/activateSettings
  [ -x "$act" ] && "$act" -u >/dev/null 2>&1 || \
    note "could not refresh the hotkey table; log out and back in"
fi

say "Writing launchd agents"
mkdir -p "$AGENTS" "$(dirname "$LOG")"

plist() { # plist <label> -- args...
  local label=$1; shift 2
  { printf '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0">\n<dict>\n'
    printf '\t<key>Label</key>\n\t<string>%s</string>\n' "$label"
    printf '\t<key>ProgramArguments</key>\n\t<array>\n'
    for a in "$@"; do printf '\t\t<string>%s</string>\n' "$a"; done
    printf '\t</array>\n'
    printf '\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n'
    printf '\t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n'
    printf '\t<key>StandardOutPath</key>\n\t<string>%s</string>\n' "$LOG"
    printf '\t<key>StandardErrorPath</key>\n\t<string>%s</string>\n' "$LOG"
    printf '</dict>\n</plist>\n'
  } > "$AGENTS/$label.plist"
  plutil -lint "$AGENTS/$label.plist" >/dev/null || die "$label.plist is malformed"
}

plist "$LABEL" -- "$DEST" \
  --vendor "$VENDOR" --product "$PRODUCT" --cid "$CONTROL" --key "$KEYCODE"

# The watchdog restarts the daemon when its once-per-attach handshake loses a
# race with the keyboard's own startup -- which is what closing the lid does.
# Optional: skipped when the script is not installed.
if [ -x "$WATCHDOG" ]; then
  plist "$WATCHDOG_LABEL" -- "$WATCHDOG"
else
  note "no $WATCHDOG; skipping the watchdog agent"
fi

say "Loading"

# `bootout` returns before launchd has finished, and bootstrapping into a
# domain that still holds the old job fails with "Bootstrap failed: 5:
# Input/output error" -- which reads like a bad plist and is not.
wait_gone() {
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    launchctl print "$1" >/dev/null 2>&1 || return 0
    sleep 1
  done
  return 1
}

for l in "$WATCHDOG_LABEL" "$LABEL"; do
  [ -f "$AGENTS/$l.plist" ] || continue
  launchctl bootout "gui/$U/$l" 2>/dev/null || true
  wait_gone "gui/$U/$l" || note "warning: $l is still loaded"
done
for l in "$LABEL" "$WATCHDOG_LABEL"; do
  [ -f "$AGENTS/$l.plist" ] || continue
  launchctl bootstrap "gui/$U" "$AGENTS/$l.plist" || die "could not load $l"
  note "loaded $l"
done

sleep 3
say "Result"
tail -n 4 "$LOG" 2>/dev/null || true
cat <<EOF

If the log says the device could not be opened, grant Input Monitoring to

  $DEST

in System Settings > Privacy & Security, then re-run:

  launchctl kickstart -k gui/$U/$LABEL

TCC is SIP-protected and cannot be granted from a script. Accessibility is
needed too, to post the key.
EOF
