{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.tangled.shuttle;

  postBuildHook = pkgs.writeShellApplication {
    name = "spindle-post-build-hook";
    text = ''
      set -f

      if [ -z "''${OUT_PATHS:-}" ]; then
        exit 0
      fi

      # OUT_PATHS is intentionally split into individual store paths for the agent
      # shellcheck disable=SC2086
      exec ${cfg.package}/bin/shuttle enqueue-built-paths $OUT_PATHS
    '';
  };
in {
  options.services.tangled.shuttle = {
    enable = lib.mkEnableOption "the shuttle guest agent";

    package = lib.mkOption {
      type = lib.types.package;
      description = "package providing the shuttle executable.";
    };
  };

  config = lib.mkIf cfg.enable {
    nix.settings.post-build-hook = "${postBuildHook}/bin/spindle-post-build-hook";

    systemd.services.shuttle = {
      description = "shuttle guest agent";
      wantedBy = ["multi-user.target"];
      wants = ["network-online.target"];
      after = [
        "local-fs.target"
        "network-online.target"
      ];
      before = ["nix-daemon.service"];
      restartIfChanged = false;
      environment = {
        NIX_PATH = lib.concatStringsSep ":" config.nix.nixPath;
      };
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/shuttle";
        Restart = "always";
        RestartSec = "1s";
      };
    };
  };
}
