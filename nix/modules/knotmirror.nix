{
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.services.tangled.knotmirror;
in
  with lib; {
    options.services.tangled.knotmirror = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Enable a tangled knot";
      };

      package = mkOption {
        type = types.package;
        description = "Package to use for the knotmirror";
      };

      tap-package = mkOption {
        type = types.package;
        description = "tap package to use for the knotmirror";
      };

      listenAddr = mkOption {
        type = types.str;
        default = "0.0.0.0:7000";
        description = "Address to listen on";
      };

      metricsListenAddr = mkOption {
        type = types.str;
        default = "127.0.0.1:7100";
        description = "Address to listen on";
      };

      adminListenAddr = mkOption {
        type = types.str;
        default = "127.0.0.1:7200";
        description = "Address to listen on";
      };

      hostname = mkOption {
        type = types.str;
        example = "my.knotmirror.com";
        description = "Hostname for the server (required)";
      };

      dbUrl = mkOption {
        type = types.str;
        example = "postgresql://...";
        description = "Database URL. postgresql expected (required)";
      };

      atpPlcUrl = mkOption {
        type = types.str;
        default = "https://plc.directory";
        description = "atproto PLC directory";
      };

      atpRelayUrl = mkOption {
        type = types.str;
        default = "https://relay1.us-east.bsky.network";
        description = "atproto relay";
      };

      fullNetwork = mkOption {
        type = types.bool;
        default = false;
        description = "Whether to automatically mirror from entire network";
      };

      knotUseSSL = mkOption {
        type = types.bool;
        default = true;
        description = "Use SSL for knot connection";
      };

      knotSSRF = mkOption {
        type = types.bool;
        default = true;
        description = "enable SSRF protection for knots";
      };

      tap = {
        port = mkOption {
          type = types.port;
          default = 7480;
          description = "Internal port to run the knotmirror tap";
        };

        dbUrl = mkOption {
          type = types.str;
          default = "sqlite:///var/lib/knotmirror-tap/tap.db";
          description = "database connection string (sqlite://path or postgres://...)";
        };
      };
    };
    config = mkIf cfg.enable {
      environment.systemPackages = [
        pkgs.git
        cfg.package
      ];

      services.redis.servers.knotmirror = {
        enable = true;
        port = 6377;
      };

      systemd.services.tap-knotmirror = {
        description = "knotmirror tap service";
        after = ["network.target"];
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          LogsDirectory = "knotmirror-tap";
          StateDirectory = "knotmirror-tap";
          Environment = [
            "TAP_BIND=:${toString cfg.tap.port}"
            "TAP_PLC_URL=${cfg.atpPlcUrl}"
            "TAP_RELAY_URL=${cfg.atpRelayUrl}"
            "TAP_RESYNC_PARALLELISM=10"
            "TAP_DATABASE_URL=${cfg.tap.dbUrl}"
            "TAP_RETRY_TIMEOUT=60s"
            "TAP_COLLECTION_FILTERS=sh.tangled.repo"
            (
              if cfg.fullNetwork
              then "TAP_SIGNAL_COLLECTION=sh.tangled.repo"
              else "TAP_FULL_NETWORK=false"
            )
          ];
          ExecStart = "${getExe cfg.tap-package} run";
        };
      };

      systemd.services.knotmirror = {
        description = "knotmirror service";
        after = ["network.target" "tap-knotmirror.service"];
        wantedBy = ["multi-user.target"];
        path = [
          pkgs.git
        ];
        serviceConfig = {
          LogsDirectory = "knotmirror";
          StateDirectory = "knotmirror";
          Environment = [
            # TODO: add environment variables
            "MIRROR_LISTEN=${cfg.listenAddr}"
            "MIRROR_HOSTNAME=${cfg.hostname}"
            "MIRROR_TAP_URL=http://localhost:${toString cfg.tap.port}"
            "MIRROR_DB_URL=${cfg.dbUrl}"
            "MIRROR_REDIS_ADDR=localhost:6377"
            "MIRROR_GIT_BASEPATH=/var/lib/knotmirror/repos"
            "MIRROR_KNOT_USE_SSL=${boolToString cfg.knotUseSSL}"
            "MIRROR_KNOT_SSRF=${boolToString cfg.knotSSRF}"
            "MIRROR_RESYNC_PARALLELISM=12"
            "MIRROR_METRICS_LISTEN=${cfg.metricsListenAddr}"
            "MIRROR_ADMIN_LISTEN=${cfg.adminListenAddr}"
            "MIRROR_SLURPER_CONCURRENCY=4"
          ];
          ExecStart = "${getExe cfg.package} serve";
          Restart = "always";
        };
      };
    };
  }
