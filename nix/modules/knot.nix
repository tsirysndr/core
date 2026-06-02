{
  config,
  pkgs,
  lib,
  ...
}: let
  cfg = config.services.tangled.knot;
in
  with lib; {
    options = {
      services.tangled.knot = {
        enable = mkOption {
          type = types.bool;
          default = false;
          description = "Enable a tangled knot";
        };

        package = mkOption {
          type = types.package;
          description = "Package to use for the knot";
        };

        appviewEndpoint = mkOption {
          type = types.str;
          default = "https://tangled.org";
          description = "Appview endpoint";
        };

        gitUser = mkOption {
          type = types.str;
          default = "git";
          description = "User that hosts git repos and performs git operations";
        };

        openFirewall = mkOption {
          type = types.bool;
          default = true;
          description = "Open port 22 in the firewall for ssh";
        };

        stateDir = mkOption {
          type = types.path;
          default = "/home/${cfg.gitUser}";
          description = "Tangled knot data directory";
        };

        repo = {
          scanPath = mkOption {
            type = types.path;
            default = cfg.stateDir;
            description = "Path where repositories are scanned from";
          };

          readme = mkOption {
            type = types.listOf types.str;
            default = [
              "README.md"
              "readme.md"
              "README"
              "readme"
              "README.markdown"
              "readme.markdown"
              "README.txt"
              "readme.txt"
              "README.rst"
              "readme.rst"
              "README.org"
              "readme.org"
              "README.asciidoc"
              "readme.asciidoc"
            ];
            description = "List of README filenames to look for (in priority order)";
          };

          mainBranch = mkOption {
            type = types.str;
            default = "main";
            description = "Default branch name for repositories";
          };
        };

        git = {
          userName = mkOption {
            type = types.str;
            default = "Tangled";
            description = "Git user name used as committer";
          };

          userEmail = mkOption {
            type = types.str;
            default = "noreply@tangled.org";
            description = "Git user email used as committer";
          };
        };

        motd = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = ''
            Message of the day

            The contents are shown as-is; eg. you will want to add a newline if
            setting a non-empty message since the knot won't do this for you.
          '';
        };

        motdFile = mkOption {
          type = types.nullOr types.path;
          default = null;
          description = ''
            File containing message of the day

            The contents are shown as-is; eg. you will want to add a newline if
            setting a non-empty message since the knot won't do this for you.
          '';
        };

        knotmirrors = mkOption {
          type = types.listOf types.str;
          default = [
            "https://mirror.tangled.network"
          ];
          description = "List of knotmirror hosts to request crawl";
        };

        server = {
          listenAddr = mkOption {
            type = types.str;
            default = "0.0.0.0:5555";
            description = "Address to listen on";
          };

          internalListenAddr = mkOption {
            type = types.str;
            default = "127.0.0.1:5444";
            description = "Internal address for inter-service communication";
          };

          owner = mkOption {
            type = types.str;
            example = "did:plc:qfpnj4og54vl56wngdriaxug";
            description = "DID of owner (required)";
          };

          dbPath = mkOption {
            type = types.path;
            default = "${cfg.stateDir}/knotserver.db";
            description = "Path to the database file";
          };

          hostname = mkOption {
            type = types.str;
            example = "my.knot.com";
            description = "Hostname for the server (required)";
          };

          plcUrl = mkOption {
            type = types.str;
            default = "https://plc.directory";
            description = "atproto PLC directory";
          };

          jetstreamEndpoint = mkOption {
            type = types.str;
            default = "wss://jetstream1.us-west.bsky.network/subscribe";
            description = "Jetstream endpoint to subscribe to";
          };

          logDids = mkOption {
            type = types.bool;
            default = true;
            description = "Enable logging of DIDs";
          };

          dev = mkOption {
            type = types.bool;
            default = false;
            description = "Enable development mode (disables signature verification)";
          };

          secureMode = mkOption {
            type = types.bool;
            default = false;
            description = "Isolate git subprocesses with Landlock (requires kernel >= 5.13)";
          };

          maxResponseKB = mkOption {
            type = types.int;
            default = 5120;
            description = "Maximum response size in kilobytes";
          };
        };

        environmentFile = mkOption {
          type = with types; nullOr path;
          default = null;
          example = "/etc/knot.env";
          description = ''
            Additional environment file as defined in {manpage}`systemd.exec(5)`.

            Sensitive secrets such as {env}`KNOT_SERVER_ADMIN_SECRET` may be
            passed to the service without making them world readable in the nix
            store.
          '';
        };
      };
    };

    config = mkIf cfg.enable {
      environment.systemPackages = [
        pkgs.git
        cfg.package
      ];

      users.users.${cfg.gitUser} = {
        isSystemUser = true;
        useDefaultShell = true;
        home = cfg.stateDir;
        createHome = true;
        group = cfg.gitUser;
      };

      users.groups.${cfg.gitUser} = {};

      services.openssh = {
        enable = true;
        extraConfig = ''
          Match User ${cfg.gitUser}
              AuthorizedKeysCommand /etc/ssh/keyfetch_wrapper
              AuthorizedKeysCommandUser nobody
              ChallengeResponseAuthentication no
              PasswordAuthentication no
        '';
      };

      environment.etc."ssh/keyfetch_wrapper" = {
        mode = "0555";
        text = ''
          #!${pkgs.stdenv.shell}
          ${cfg.package}/bin/knot keys \
            -output authorized-keys \
            -internal-api "http://${cfg.server.internalListenAddr}" \
            -git-dir "${cfg.repo.scanPath}" \
            -log-path /tmp/knotguard.log \
            ${optionalString cfg.server.secureMode "-secure-mode -guard-path /run/wrappers/bin/knot"}
        '';
      };

      # In secure mode, the guard subcommand (run via sshd's forced command as
      # the git user) needs CAP_SETUID to drop to the virtual UID before exec'ing
      # git. security.wrappers installs a setcap'd wrapper around the binary.
      security.wrappers = mkIf cfg.server.secureMode {
        knot = {
          source = "${cfg.package}/bin/knot";
          owner = "root";
          group = cfg.gitUser;
          permissions = "u+rx,g+x";
          capabilities = "cap_setuid,cap_setgid,cap_chown+eip";
        };
      };

      systemd.services.knot = {
        description = "knot service";
        after = ["network-online.target" "sshd.service"];
        wants = ["network-online.target"];
        wantedBy = ["multi-user.target"];
        enableStrictShellChecks = true;

        preStart = let
          setMotd =
            if cfg.motdFile != null && cfg.motd != null
            then throw "motdFile and motd cannot be both set"
            else ''
              ${optionalString (cfg.motdFile != null) "cat ${cfg.motdFile} > ${cfg.stateDir}/motd"}
              ${optionalString (cfg.motd != null) ''printf "${cfg.motd}" > ${cfg.stateDir}/motd''}
            '';
        in ''
          mkdir -p "${cfg.repo.scanPath}"
          mkdir -p "${cfg.stateDir}/.config/git"
          cat > "${cfg.stateDir}/.config/git/config" << EOF
          [user]
              name = ${cfg.git.userName}
              email = ${cfg.git.userEmail}
          [receive]
              advertisePushOptions = true
          [uploadpack]
              allowFilter = true
              allowReachableSHA1InWant = true
          [safe]
              directory = *
          EOF
          ${setMotd}
          chown ${cfg.gitUser}:${cfg.gitUser} "${cfg.repo.scanPath}"
          chown -R ${cfg.gitUser}:${cfg.gitUser} "${cfg.stateDir}/.config"
          ${optionalString cfg.server.secureMode ''
            # secure mode runs git subprocesses as virtual UIDs that are not in
            # the git group. they need execute (traverse) permission on the
            # state dir so they can resolve $HOME/.config/git/config; read
            # permission is intentionally withheld so they can't list other
            # repos by name.
            chmod o+x "${cfg.stateDir}"
          ''}
          ${optionalString (cfg.motdFile != null || cfg.motd != null) "chown ${cfg.gitUser}:${cfg.gitUser} ${cfg.stateDir}/motd"}
          ${optionalString cfg.server.secureMode ''
            ${cfg.package}/bin/knot migrate-isolation \
              --force \
              --git-dir "${cfg.repo.scanPath}" \
              --db "${cfg.server.dbPath}" \
              --internal-api "${cfg.server.internalListenAddr}"
            chown ${cfg.gitUser}:${cfg.gitUser} "${cfg.server.dbPath}"
          ''}
        '';

        serviceConfig =
          {
            User = cfg.gitUser;
            PermissionsStartOnly = true;
            WorkingDirectory = cfg.stateDir;
            Environment = [
              "PATH=${lib.makeBinPath [pkgs.bash pkgs.git pkgs.coreutils]}:/run/current-system/sw/bin"
              "KNOT_REPO_SCAN_PATH=${cfg.repo.scanPath}"
              "KNOT_REPO_README=${concatStringsSep "," cfg.repo.readme}"
              "KNOT_REPO_MAIN_BRANCH=${cfg.repo.mainBranch}"
              "KNOT_GIT_USER_NAME=${cfg.git.userName}"
              "KNOT_GIT_USER_EMAIL=${cfg.git.userEmail}"
              "APPVIEW_ENDPOINT=${cfg.appviewEndpoint}"
              "KNOT_SERVER_INTERNAL_LISTEN_ADDR=${cfg.server.internalListenAddr}"
              "KNOT_SERVER_LISTEN_ADDR=${cfg.server.listenAddr}"
              "KNOT_SERVER_DB_PATH=${cfg.server.dbPath}"
              "KNOT_SERVER_HOSTNAME=${cfg.server.hostname}"
              "KNOT_SERVER_PLC_URL=${cfg.server.plcUrl}"
              "KNOT_SERVER_JETSTREAM_ENDPOINT=${cfg.server.jetstreamEndpoint}"
              "KNOT_SERVER_OWNER=${cfg.server.owner}"
              "KNOT_MIRRORS=${concatStringsSep "," cfg.knotmirrors}"
              "KNOT_SERVER_LOG_DIDS=${
                if cfg.server.logDids
                then "true"
                else "false"
              }"
              "KNOT_SERVER_DEV=${
                if cfg.server.dev
                then "true"
                else "false"
              }"
              "KNOT_SERVER_MAX_RESPONSE_KB=${toString cfg.server.maxResponseKB}"
              "KNOT_SERVER_SECURE_MODE=${
                if cfg.server.secureMode
                then "true"
                else "false"
              }"
            ];
            EnvironmentFile = mkIf (cfg.environmentFile != null) cfg.environmentFile;
            ExecStart = "${cfg.package}/bin/knot server";
            Restart = "always";
          }
          // optionalAttrs cfg.server.secureMode {
            AmbientCapabilities = ["CAP_CHOWN" "CAP_SETUID" "CAP_SETGID"];
            CapabilityBoundingSet = ["CAP_CHOWN" "CAP_SETUID" "CAP_SETGID"];
          };
      };

      networking.firewall.allowedTCPPorts = mkIf cfg.openFirewall [22];
    };
  }
