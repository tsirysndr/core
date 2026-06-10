{
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.services.tangled.zoekt;
in
  with lib; {
    options.services.tangled.zoekt = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Enable a tangled zoekt node";
      };

      package = mkOption {
        type = types.package;
        description = "zoekt-tngl-indexserver package to use for zoekt node";
      };

      zoekt-webserver-package = mkOption {
        type = types.package;
        description = "zoekt-webserver package to use for zoekt node";
      };

      listenAddr = mkOption {
        type = types.str;
        default = ":6060";
        description = "zoekt-tngl-indexserver listen address";
      };

      webListenAddr = mkOption {
        type = types.str;
        default = ":6070";
        description = "zoekt-webserver listen address";
      };

      atpPlcUrl = mkOption {
        type = types.str;
        default = "https://plc.directory";
        description = "atproto PLC directory";
      };

      appviewUrl = mkOption {
        type = types.str;
        default = "https://tangled.org";
        description = "Tangled appview URL";
      };

      indexDir = mkOption {
        type = types.path;
        default = "/var/lib/zoekt-tnglserver/index";
        description = "zoekt index directory";
      };

      indexConcurrency = mkOption {
        type = types.int;
        default = 4;
        description = "Maximum number of concurrent index jobs to run";
      };

      indexQueueSize = mkOption {
        type = types.int;
        default = 100;
        description = "Maximum number of index jobs queue up";
      };
    };
    config = mkIf cfg.enable {
      # environment.systemPackages = [
      #   pkgs.git
      #   cfg.package
      # ];

      systemd.services.zoekt-webserver = {
        description = "tangled zoekt-webserver";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          LogsDirectory = "zoekt-webserver";
          StateDirectory = "zoekt-webserver";
          ExecStart = "${getExe cfg.zoekt-webserver-package} -index ${cfg.indexDir} -rpc";
        };
      };

      systemd.services.zoekt-tngl-indexserver = {
        description = "tangled zoekt index server service";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];
        path = [
          pkgs.git
          cfg.package
        ];
        serviceConfig = {
          LogsDirectory = "zoekt-tngl-indexserver";
          StateDirectory = "zoekt-tngl-indexserver";
          Environment = [
            "TANGLED_ZOEKT_INDEX_DIR=${cfg.indexDir}"
            "TANGLED_ZOEKT_INDEX_CONCURRENCY=${toString cfg.indexConcurrency}"
            "TANGLED_ZOEKT_INDEX_QUEUE_SIZE=${toString cfg.indexQueueSize}"
            "TANGLED_ZOEKT_PLC_URL=${cfg.atpPlcUrl}"
            "TANGLED_ZOEKT_APPVIEW_URL=${cfg.appviewUrl}"
          ];
          ExecStart = "${getExe cfg.package} serve";
          Restart = "always";
        };
      };
    };
  }
