{
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.services.tangled.deliberi;
in
  with lib; {
    options.services.tangled.deliberi = {
      enable = mkEnableOption "tangled deliberi notification + email service";

      package = mkOption {
        type = types.package;
        description = "deliberi package to run";
      };

      listenAddr = mkOption {
        type = types.str;
        default = "0.0.0.0:6565";
        description = "address the xrpc + health server binds to";
      };

      hostname = mkOption {
        type = types.str;
        description = "public hostname; derives deliberi's did:web identity";
      };

      dbPath = mkOption {
        type = types.str;
        default = "/var/lib/deliberi/deliberi.db";
        description = "path to deliberi's sqlite database";
      };

      plcUrl = mkOption {
        type = types.str;
        default = "https://plc.directory";
      };

      jetstreamEndpoint = mkOption {
        type = types.str;
        default = "wss://jetstream1.us-east.bsky.network/subscribe";
      };

      bobbinApiUrl = mkOption {
        type = types.str;
        default = "https://api.tangled.org";
        description = "base url hosting subscription.listRecipients";
      };

      baseUrl = mkOption {
        type = types.str;
        default = "https://tangled.org";
        description = "public frontend url used for links in digest emails";
      };

      dev = mkOption {
        type = types.bool;
        default = false;
      };

      environmentFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "file with secret env vars (DELIBERI_RESEND_API_KEY, DELIBERI_PDS_ADMIN_SECRET, ...)";
      };
    };

    config = mkIf cfg.enable {
      systemd.services.deliberi = {
        description = "tangled deliberi notification + email service";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          LogsDirectory = "deliberi";
          StateDirectory = "deliberi";
          EnvironmentFile = mkIf (cfg.environmentFile != null) cfg.environmentFile;
          Environment = [
            "DELIBERI_LISTEN_ADDR=${cfg.listenAddr}"
            "DELIBERI_HOSTNAME=${cfg.hostname}"
            "DELIBERI_DB_PATH=${cfg.dbPath}"
            "DELIBERI_PLC_URL=${cfg.plcUrl}"
            "DELIBERI_JETSTREAM_ENDPOINT=${cfg.jetstreamEndpoint}"
            "DELIBERI_BOBBIN_API_URL=${cfg.bobbinApiUrl}"
            "DELIBERI_BASE_URL=${cfg.baseUrl}"
            "DELIBERI_DEV=${boolToString cfg.dev}"
          ];
          ExecStart = "${getExe cfg.package} serve";
          Restart = "always";
        };
      };
    };
  }
