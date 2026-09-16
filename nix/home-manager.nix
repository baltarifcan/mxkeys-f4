{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.mxkeys-f4;
  package = self.packages.${pkgs.stdenv.hostPlatform.system}.mxkeys-f4;

  # A fixed path, not the /nix/store one. TCC records the path a grant belongs
  # to, and the store path changes on every rebuild. See scripts/install-signed.sh.
  installDir = "${config.home.homeDirectory}/.local/libexec";
  daemon = "${installDir}/mxkeys-f4d";

  hotKeyValue = lib.generators.toPlist { escape = true; } {
    enabled = true;
    value = {
      # (character, virtual key code, modifier mask). 65535 is "no character".
      # The mask is written as 0 and reads back as 0x800000 -- the window server
      # adds the function-key bit itself for an F-key binding, and a real F-key
      # event carries the same bit, so the two still match.
      parameters = [
        65535
        cfg.keyCode
        0
      ];
      type = "standard";
    };
  };
in
{
  options.programs.mxkeys-f4 = {
    enable = lib.mkEnableOption "the MX Keys F4 daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = package;
      defaultText = lib.literalMD "the flake's own build";
      description = "The mxkeys-f4 package to install.";
    };

    vendorId = lib.mkOption {
      type = lib.types.int;
      default = 1133; # 0x046d
      description = "USB vendor id of the keyboard.";
    };

    productId = lib.mkOption {
      type = lib.types.int;
      default = 45915; # 0xb35b
      description = "USB product id of the keyboard. 0xb35b is the universal MX Keys.";
    };

    controlId = lib.mkOption {
      type = lib.types.int;
      default = 225; # 0x00e1
      description = ''
        HID++ control to divert. 0x00e1 is "Dashboard (Launchpad) / Action
        Center", which is the F4 key on this layout.
      '';
    };

    keyCode = lib.mkOption {
      type = lib.types.int;
      default = 79; # F18
      description = ''
        Virtual key code the daemon posts when the control is pressed. F18 by
        default, because no Apple or Logitech keyboard has one, so binding it
        costs no real key.
      '';
    };

    symbolicHotKey = lib.mkOption {
      type = lib.types.nullOr lib.types.int;
      default = 160;
      description = ''
        macOS symbolic hotkey to bind `keyCode` to, or null to leave shortcuts
        alone and bind the key yourself.

        160 was "Show Launchpad" up to macOS 15. Apple kept the id and repointed
        it at Spotlight's Apps view; KeyboardSettings' own shortcut table lists
        it under Spotlight as "Show Apps" on macOS 26 and later.
      '';
    };

    verbose = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Log every keypress to the agent's log file.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = pkgs.stdenv.hostPlatform.isDarwin;
        message = "programs.mxkeys-f4 is macOS only.";
      }
    ];

    home.packages = [ cfg.package ];

    home.activation.mxkeysF4 = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      run ${pkgs.bash}/bin/bash ${../scripts/install-signed.sh} \
        ${cfg.package}/bin/mxkeys-f4d ${daemon}

      ${lib.optionalString (cfg.symbolicHotKey != null) ''
        # -dict-add, not a plain write: AppleSymbolicHotKeys is one dictionary
        # holding every keyboard shortcut macOS has, and writing the key would
        # replace all of them.
        run /usr/bin/defaults write com.apple.symbolichotkeys AppleSymbolicHotKeys \
          -dict-add ${toString cfg.symbolicHotKey} ${lib.escapeShellArg hotKeyValue}

        # Registers the binding with the window server now; without it the
        # change sits in the plist until the next login.
        run /System/Library/PrivateFrameworks/SystemAdministration.framework/Resources/activateSettings -u \
          > /dev/null 2>&1 || true
      ''}
    '';

    launchd.agents.mxkeys-f4 = {
      enable = true;
      config = {
        ProgramArguments = [
          daemon
          "--vendor"
          (toString cfg.vendorId)
          "--product"
          (toString cfg.productId)
          "--cid"
          (toString cfg.controlId)
          "--key"
          (toString cfg.keyCode)
        ]
        ++ lib.optional cfg.verbose "--verbose";

        RunAtLoad = true;
        # The daemon is the only thing standing between F4 and typing F12, so it
        # comes back if it ever dies. ThrottleInterval keeps a persistent failure
        # (no Input Monitoring, say) from becoming a spin loop.
        KeepAlive = true;
        ThrottleInterval = 10;

        StandardOutPath = "${config.home.homeDirectory}/Library/Logs/mxkeys-f4.log";
        StandardErrorPath = "${config.home.homeDirectory}/Library/Logs/mxkeys-f4.log";
      };
    };
  };
}
