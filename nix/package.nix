{
  lib,
  rustPlatform,
}:

# A plain Rust build. Nothing here needs the system Xcode toolchain: the daemon
# links only public frameworks (CoreFoundation, IOKit, CoreGraphics), all of
# which nixpkgs' Apple SDK provides. That is the reason this is a derivation
# while alt-tab-unlocked cannot be one.

rustPlatform.buildRustPackage {
  pname = "mxkeys-f4";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        base = baseNameOf path;
      in
      !(base == "target" || base == ".git" || base == "result");
  };

  cargoLock.lockFile = ../Cargo.lock;

  # No tests: everything this does is talk to a physical keyboard.
  doCheck = false;

  meta = {
    description = "Give the Logitech MX Keys F4 key back on macOS without giving up F12";
    homepage = "https://github.com/baltarifcan/mxkeys-f4";
    license = lib.licenses.mit;
    platforms = lib.platforms.darwin;
    mainProgram = "mxkeys-f4d";
  };
}
