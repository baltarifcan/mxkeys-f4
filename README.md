# mxkeys-f4

Give the Logitech MX Keys **F4** key back on macOS — without giving up **F12**,
and without running Logi Options+.

A single ~2.5 MB daemon, no dependencies, no GUI, no menu bar item, no updater.
It replaces a 289 MB two-process install that took **10.4 seconds** after login
before the key started working. This takes **0.2**.

```
                    F4 pressed
                         │
     ┌───────────────────┴───────────────────┐
     │  without this daemon                  │  with it
     │                                       │
  HID usage 0x45                      HID++ 0x1B04 notification
  (which is F12)                      on vendor page 0xFF43
     │                                       │
  macOS types F12                     mxkeys-f4d posts F18
                                             │
                                      "Show Apps" opens
```

## The problem

The universal MX Keys (`046d:b35b` — *not* the separate "MX Keys for Mac" SKU)
resolves Fn entirely in firmware. macOS never receives an Fn modifier: tapping
Fn alone produces zero HID events, because the report descriptor has no
`AppleVendorTopCase` page to carry one.

What the key in the F4 position actually sends is plain HID keyboard usage
`0x45`, which is F12. So does Fn+F12. Captured off the wire, the two presses are
byte-identical:

| press | report |
|---|---|
| bare F4 | `01 00 45 00 00 00 00 00` |
| Fn + F12 | `01 00 45 00 00 00 00 00` |

This was never a macOS 26 "Launchpad was removed" regression. The descriptor
declares only Keyboard page `0x00`–`0xA4`, Consumer `0x0001`–`0x028C`, and
Logitech's HID++ vendor page `0xFF43`. Apple's Launchpad usage lives on
`AppleVendorKeyboard` (`0xFF01`), which this keyboard does not have, and
Launchpad's Consumer usage `0x29F` is above its declared maximum of `0x28C`. The
hardware never had a way to say "Launchpad". There is exactly one MX Keys
firmware release, 12.01.13 from March 2021.

**Consequence:** no remapper working at the macOS layer — `hidutil`, a ByHost
`com.apple.keyboard.modifiermapping` entry, Karabiner — can tell the two presses
apart, because by the time they reach the Mac they are the same byte. Any fix at
that layer costs you literal F12 on that keyboard.

## Why a daemon

The distinction does still exist, just not on the keyboard channel. HID++
feature `0x1B04` can **divert** a control: the key stops emitting its HID usage
altogether and the device sends a notification naming the control instead.
Divert only F4 and usage `0x45` is never touched, so Fn+F12 keeps typing a real
F12.

Two routes that would have needed no resident process at all were checked
against the hardware, and both are dead:

- **`0x1C00` (Persistent Remappable Action)** — stores a remap in the keyboard's
  own non-volatile memory. **Absent** from this firmware. Queried directly
  against the root feature table.
- **`0x1B04`'s `remap` field** — accepted, echoed, and read back correctly, then
  **ignored**. Tested with two different targets (`0x00EA` App/Menu and `0x00E0`
  Mission Control); the key emitted `01 00 45` either way. The `reprog` flag on
  the control is aspirational.

`0x4531 MultiPlatform` is not a way out either: the host already runs platform
index 1 (`osMask 0x20`, macOS), and the alternatives are Windows/iOS/Android,
which break Cmd/Option.

So diverting is the only mechanism that works, and something has to be resident
to receive the notifications. This is that something, kept as small as the
platform permits.

## What it costs

Measured with `footprint`, not `ps` — RSS counts shared framework pages that
every process on the machine already maps, which is why this looks like 8 MB
there and 2.5 MB in reality.

| | binary | phys_footprint | time to working |
|---|---|---|---|
| **mxkeys-f4d** | 330 KB | **2560 KB** | **0.21 s** |
| Logi Options+ | — | 289 MB (2 procs) | 10.38 s |

"Time to working" is the honest metric: launch the thing, then poll the
*keyboard* until it reports the control diverted. That is the moment F4 stops
sending `0x45`.

There is nothing left to optimise. An empty program that does nothing but
`CFRunLoopRun()` already costs 1632 KB — **64% of the total**. Of the 928 KB on
top, IOKit and the HID machinery are ~750 KB and CoreGraphics is 176 KB
(measured by stubbing the action out entirely). The program's own logic rounds
to zero. Rewriting it in C or Swift moves the number by under 3%:

| implementation | binary | phys_footprint |
|---|---|---|
| Rust | 313 KB | 2560 KB |
| Swift, no Foundation | 56 KB | 2581 KB |
| C | 35 KB | 2619 KB |
| Swift with Foundation | 84 KB | 2880 KB |

Rust was chosen for being marginally lowest and, more usefully, for linking
only public frameworks, so `cargo build` needs nothing beyond the Xcode Command
Line Tools.

## Install

Requires macOS, a paired MX Keys, and a Rust toolchain (`mise use -g rust`, or
rustup).

```sh
git clone https://github.com/baltarifcan/mxkeys-f4
cd mxkeys-f4
./install.sh
```

That builds the daemon, installs a **code-signed** copy at
`~/.local/libexec/mxkeys-f4d`, binds macOS symbolic hotkey 160 ("Show Apps" on
macOS 26+, "Show Launchpad" before it) to F18, and loads the LaunchAgents.

`./install.sh --uninstall` removes the agents and the installed daemon.

### Two permissions, granted once

The daemon needs **Input Monitoring** to read the HID++ channel and
**Accessibility** to post the key. Both are TCC grants and cannot be scripted —
SIP protects that database. Grant them to `~/.local/libexec/mxkeys-f4d` in
System Settings → Privacy & Security.

They survive upgrades. `scripts/install-signed.sh` generates a self-signed
certificate in your login keychain and signs the installed copy with it, so the
designated requirement TCC records is

```
identifier "mxkeys-f4d" and certificate leaf = H"<stable>"
```

rather than a cdhash that changes on every rebuild. The certificate is
deliberately *not* added to the trust store — `codesign` signs happily with an
untrusted identity, the signature is just as stable, and skipping the trust step
means no `sudo` and no authorisation dialog. Same reasoning, and the same
approach, as [alt-tab-unlocked](https://github.com/baltarifcan/alt-tab-unlocked).

The binary is copied to a fixed path rather than run from the build directory
for the same reason: TCC records the path, and a build directory is not stable.

### Options

The constants at the top of `install.sh`:

```sh
VENDOR=1133      # 0x046d, Logitech
PRODUCT=45915    # 0xb35b, the universal MX Keys
CONTROL=225      # 0x00e1, the F4 control
KEYCODE=79       # F18
```

The daemon takes the same values as flags, so `mxkeys-f4d --help` is the
reference.

The daemon takes the same settings as flags — `mxkeys-f4d --help`. Nothing about
it is MX Keys specific beyond the defaults; any Logitech device exposing
`0x1B04` should work, though the control ids differ per device.

## Trade-offs, stated plainly

- **Something has to run.** That is forced by the hardware, not a design choice;
  see the two dead ends above. The daemon is event-driven and idles at 0.0% CPU.
- **If it is not running, F4 types F12.** The divert is set non-persistently on
  purpose. `0x1B04` can make a divert survive power cycles, but then a stopped
  daemon would leave F4 completely dead instead of merely wrong. Failing soft is
  better. `SIGTERM` clears the divert on the way out.
- **It only fixes one key.** Deliberately. This is not a Logitech control panel.
  For that, see [OpenLogi](https://github.com/AprilNEA/OpenLogi) — though note
  it could not enumerate this keyboard when tested: `0xb35b` appears in its
  device-channel registry only as a test asserting `lookup(&direct(0xb35b)) ==
  None`, and its HID++ 1.0 register path targets Bolt/Unifying receivers rather
  than BLE-direct keyboards.

## Notes for anyone doing HID++ on macOS

Three things cost real time and are not written down anywhere obvious:

1. **This keyboard declares only the long report.** Report `0x11` (19-byte
   payload) exists under `0xFF43`; there is no short `0x10` at all. Code that
   assumes a short report gets `kIOReturnNotFound` from `IOHIDDeviceSetReport`
   and no useful diagnosis.
2. **`IOHIDDeviceSetReport` wants the report id twice** — as the `reportID`
   argument *and* as byte 0 of the buffer. Passing it only as the argument
   returns `kIOReturnSuccess` and produces no reply at all.
3. **Input reports arrive with the report id already at byte 0**, so prepending
   the callback's `reportID` gives you a duplicate.

And one Rust-specific trap, documented at the `State` struct: the attach handler
pumps the run loop while awaiting a reply, which re-enters the input-report
callback, so two handlers are live on the same object. Holding a `&mut` across
that is UB, and with optimisation on the compiler caches the flag in a register
and the wait loop never observes the reply — presenting as "device does not
expose feature `0x1B04`" against a device that plainly does. Shared references
plus `Cell` fix it.

## Licence

MIT. See [LICENCE.md](LICENCE.md).
