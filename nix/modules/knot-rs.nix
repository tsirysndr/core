{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.tangled.knot-rs;

  inherit (lib) literalExpression mkEnableOption mkOption types;

  settingsFormat = pkgs.formats.toml {};

  addrType = types.strMatching "^([[][0-9a-fA-F:]+[]]|[^:]+):[0-9]+$";
  absPathType = types.strMatching "^/.+";

  portOf = addr: lib.toInt (lib.last (lib.splitString ":" addr));
  hostOf = addr: lib.concatStringsSep ":" (lib.init (lib.splitString ":" addr));
  isLoopback = addr: lib.elem (hostOf addr) ["127.0.0.1" "[::1]"];

  inherit (cfg.settings) server tls;

  sshPort = portOf server.ssh_listen_addr;
  listenPort = portOf server.listen_addr;
  internalPort = portOf server.internal_listen_addr;

  tlsEnabled = tls.acme_enabled || tls.cert_path != null;

  publicTcpPorts =
    lib.optional (!isLoopback server.ssh_listen_addr) sshPort
    ++ lib.optional (!isLoopback server.listen_addr) listenPort
    ++ lib.optional (tls.mtls_enabled && !isLoopback server.internal_listen_addr) internalPort;

  publicUdpPorts =
    lib.optional (tlsEnabled && tls.http3 && !isLoopback server.listen_addr) listenPort;

  bindsPrivilegedPort =
    lib.any (port: port < 1024)
    ([sshPort listenPort] ++ lib.optional tls.mtls_enabled internalPort);

  stateDirs = lib.unique (
    [cfg.stateDir cfg.settings.repo.scan_path]
    ++ lib.optional (cfg.settings.lfs.store_path != null) cfg.settings.lfs.store_path
    ++ lib.optional tls.acme_enabled tls.acme_cache_dir
  );

  keyDirs = lib.subtractLists stateDirs (lib.unique [
    (dirOf cfg.settings.secrets.sealed_key_file)
    (dirOf server.ssh_host_key_file)
  ]);

  writablePaths = stateDirs ++ keyDirs;

  usesHomePath =
    lib.any
    (path: lib.any (prefix: lib.hasPrefix prefix "${path}/") ["/home/" "/root/"])
    writablePaths;

  populated = lib.filterAttrsRecursive (_: value: value != null) cfg.settings;

  rendered =
    settingsFormat.generate "knot.toml"
    (lib.filterAttrs (_: value: value != {}) populated);

  configFile =
    if pkgs.stdenv.buildPlatform.canExecute pkgs.stdenv.hostPlatform
    then
      pkgs.runCommandLocal "knot-config.toml" {
        nativeBuildInputs = [cfg.package];
      } ''
        knot-server validate --config-only ${rendered}
        ln -s ${rendered} $out
      ''
    else rendered;
in {
  _class = "nixos";

  options.services.tangled.knot-rs = {
    enable = mkEnableOption "the knot git server";

    package = mkOption {
      type = types.package;
      description = "Package providing the knot-server binary";
    };

    migratePackage = mkOption {
      type = types.package;
      description = "Package providing the knot-migrate binary";
    };

    installMigrateTool = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Whether to instlal {option}`migratePackage` system-wide.
        Only needed if doing a one-time migration from the Go knot.
      '';
    };

    user = mkOption {
      type = types.str;
      default = "knot";
      description = "User the knot runs as and the owner of the repositories";
    };

    group = mkOption {
      type = types.str;
      default = cfg.user;
      description = "Group the knot runs as";
    };

    stateDir = mkOption {
      type = absPathType;
      default = "/var/lib/knot";
      description = "Directory the knot stores its repositories, sealed key, and ssh host key in";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Whether to open the port of each listen address that isn't loopback,
        plus the matching UDP port when HTTP3 serves over TLS.
      '';
    };

    environmentFile = mkOption {
      type = types.nullOr absPathType;
      default = null;
      example = "/etc/secrets/knot.env";
      description = ''
        Environment file as defined in {manpage}`systemd.exec(5)`,
        which sets the master key and any other secret
        so they stay out of the nix store.
        Every `KNOT_*` variable it sets
        also overrides the matching key in {option}`settings`.
      '';
    };

    settings = mkOption {
      type = types.submodule {
        freeformType = settingsFormat.type;

        options = {
          server = {
            hostname = mkOption {
              type = types.str;
              example = "knot.oyster.cafe";
              description = "Public hostname, which is also the knot's did:web identity";
            };

            admins = mkOption {
              type = types.nonEmptyListOf types.str;
              example = ["did:plc:boltless"];
              description = ''
                DIDs with knot-admin authority.
                The knot reports the first entry as its service owner,
                so reordering this list changes the owner it advertises.
              '';
            };

            listen_addr = mkOption {
              type = addrType;
              default = "127.0.0.1:5555";
              description = ''
                Address the HTTP surface listens on.
                The module default suits a reverse proxy in front,
                while the binary's own default is `[::]:5555`.
              '';
            };

            internal_listen_addr = mkOption {
              type = addrType;
              default = "[::1]:5444";
              description = "Address the mTLS admin surface listens on when {option}`settings.tls.mtls_enabled` is set";
            };

            ssh_listen_addr = mkOption {
              type = addrType;
              default = "[::]:2222";
              description = ''
                Address the knot's own ssh server listens on.
                Moving this to port 22 collides with {option}`services.openssh`
                unless that also moves.
              '';
            };

            ssh_host_key_file = mkOption {
              type = absPathType;
              default = "${cfg.stateDir}/ssh_host_ed25519_key";
              defaultText = literalExpression ''"''${stateDir}/ssh_host_ed25519_key"'';
              description = ''
                Private ssh host key the knot presents.
                The knot creates one on first start when the file is absent,
                so its directory must be writable.
                Keep this off the nix store.
              '';
            };
          };

          acl.admission = mkOption {
            type = types.enum ["closed" "open"];
            default = "closed";
            description = "Whether repository creation needs knot membership";
          };

          repo.scan_path = mkOption {
            type = absPathType;
            default = "${cfg.stateDir}/repos";
            defaultText = literalExpression ''"''${stateDir}/repos"'';
            description = "Directory the knot serves repositories from";
          };

          git.object_format = mkOption {
            type = types.enum ["sha1" "sha256"];
            default = "sha256";
            description = "Object format for repositories the knot creates";
          };

          secrets = {
            sealed_key_file = mkOption {
              type = absPathType;
              default = "${cfg.stateDir}/knot.sealed";
              defaultText = literalExpression ''"''${stateDir}/knot.sealed"'';
              description = ''
                Sealed store for the knot signing key.
                The knot creates one on first start when the file is absent,
                so its directory must be writable.
              '';
            };

            master_key_env = mkOption {
              type = types.strMatching "^[A-Z_][A-Z0-9_]*$";
              default = "KNOT_MASTER_KEY";
              description = ''
                Name of the environment variable with the base64 master key that unseals
                {option}`settings.secrets.sealed_key_file`.
                Set the value itself in {option}`environmentFile`.
                Losing it makes every sealed key unreadable.
              '';
            };
          };

          atproto.plc_directory = mkOption {
            type = types.str;
            example = "https://plc.directory";
            description = "atproto PLC directory. This has no default so that the plcdir is an explicit choice.";
          };

          xrpc.trusted_proxy_header = mkOption {
            type = types.nullOr types.str;
            default = null;
            example = "x-forwarded-for";
            description = ''
              Header a trusted reverse proxy appends the client address to.
              Rate limiting keys every request on the proxy's own address while this is null.
              Only set it if a trusted proxy overwrites the header,
              since a client can forge it otherwise.
            '';
          };

          lfs.store_path = mkOption {
            type = types.nullOr absPathType;
            default = null;
            description = "Directory for Git LFS objects. The knot won't serve LFS while this is null.";
          };

          tls = {
            cert_path = mkOption {
              type = types.nullOr absPathType;
              default = null;
              example = "/etc/knot/tls/cert.pem";
              description = ''
                Certificate chain the knot presents.
                The knot serves plain HTTP
                while this and {option}`settings.tls.acme_enabled` are both unset.
                That suits a reverse proxy in front.
              '';
            };

            key_path = mkOption {
              type = types.nullOr absPathType;
              default = null;
              example = "/etc/knot/tls/key.pem";
              description = ''
                Private key for {option}`settings.tls.cert_path`.
                Set both or neither.
                Keep this off the nix store.
              '';
            };

            http3 = mkOption {
              type = types.bool;
              default = true;
              description = "Whether to serve HTTP/3 over QUIC on the UDP port matching {option}`settings.server.listen_addr`";
            };

            acme_enabled = mkOption {
              type = types.bool;
              default = false;
              description = "Whether to obtain certificates over ACME instead of reading {option}`settings.tls.cert_path`";
            };

            acme_cache_dir = mkOption {
              type = absPathType;
              default = "${cfg.stateDir}/acme";
              defaultText = literalExpression ''"''${stateDir}/acme"'';
              description = "Directory for the ACME account key and issued certificates";
            };

            acme_contact = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "nel@oyster.cafe";
              description = "Contact email the knot registers the ACME account with, required when ACME is enabled";
            };

            acme_staging = mkOption {
              type = types.bool;
              default = false;
              description = ''
                Whether to use the Let's Encrypt staging directory.
                Set it while testing so a typo doesn't exhaust the production rate limit.
              '';
            };

            mtls_enabled = mkOption {
              type = types.bool;
              default = false;
              description = "Whether to serve the mTLS admin surface on {option}`settings.server.internal_listen_addr`";
            };

            mtls_client_ca_path = mkOption {
              type = types.nullOr absPathType;
              default = null;
              example = "/etc/knot/tls/admin-ca.pem";
              description = "CA that signs admin client certificates, required when mTLS is enabled";
            };

            mtls_admin_spki_pin = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = "Base64 SHA-256 SPKI pin of the admin client certificate, required when mTLS is enabled";
            };
          };
        };
      };

      description = ''
        Configuration the module renders to `/etc/knot/config.toml`.
        The knot reads that file on startup.
        Keys beyond the ones declared here pass through unchanged,
        and `knot-server validate --config-only` checks the result at build time
        when the build platform can run the knot binary.
        Run `nix run .#knot-rs -- config-template` for the full key list.
        Put secrets in {option}`environmentFile`.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.environmentFile != null;
        message = "services.tangled.knot-rs.environmentFile must be set, since the knot reads its master key from ${cfg.settings.secrets.master_key_env} in the environment and won't start without it";
      }
      {
        assertion = !config.services.openssh.enable || !(lib.elem sshPort config.services.openssh.ports);
        message = "services.tangled.knot-rs.settings.server.ssh_listen_addr takes port ${toString sshPort}, which services.openssh already listens on";
      }
      {
        assertion = (tls.cert_path == null) == (tls.key_path == null);
        message = "services.tangled.knot-rs.settings.tls.cert_path and tls.key_path must both be set or both unset";
      }
      {
        assertion = !(tls.acme_enabled && tls.cert_path != null);
        message = "services.tangled.knot-rs.settings.tls.acme_enabled can't combine with a static tls.cert_path";
      }
      {
        assertion = !tls.acme_enabled || tls.acme_contact != null;
        message = "services.tangled.knot-rs.settings.tls.acme_contact is required when tls.acme_enabled is set";
      }
      {
        assertion = !tls.acme_enabled || !isLoopback server.listen_addr;
        message = "services.tangled.knot-rs.settings.tls.acme_enabled needs a certificate authority to reach settings.server.listen_addr, and ${server.listen_addr} is loopback";
      }
      {
        assertion = !tls.mtls_enabled || tlsEnabled;
        message = "services.tangled.knot-rs.settings.tls.mtls_enabled requires a certificate from tls.cert_path or ACME";
      }
      {
        assertion = !tls.mtls_enabled || (tls.mtls_client_ca_path != null && tls.mtls_admin_spki_pin != null);
        message = "services.tangled.knot-rs.settings.tls.mtls_enabled requires tls.mtls_client_ca_path and tls.mtls_admin_spki_pin";
      }
    ];

    warnings =
      lib.optional (tls.acme_enabled && listenPort != 443)
      "services.tangled.knot-rs validates over TLS-ALPN-01, which a certificate authority reaches on TCP 443, and settings.server.listen_addr uses port ${toString listenPort}. Map 443 to that port.";

    environment.systemPackages =
      [cfg.package]
      ++ lib.optional cfg.installMigrateTool cfg.migratePackage;

    environment.etc."knot/config.toml".source = configFile;

    users.users.${cfg.user} = {
      isSystemUser = true;
      home = cfg.stateDir;
      inherit (cfg) group;
    };

    users.groups.${cfg.group} = {};

    systemd.tmpfiles.settings."10-knot-rs" = lib.genAttrs stateDirs (path: {
      d = {
        mode =
          if path == tls.acme_cache_dir
          then "0700"
          else "0750";
        inherit (cfg) user group;
      };
    });

    systemd.services.knot-rs = {
      description = "knot git server";
      after = ["network-online.target"];
      wants = ["network-online.target"];
      wantedBy = ["multi-user.target"];

      restartTriggers = [configFile];

      startLimitIntervalSec = 60;
      startLimitBurst = 5;

      serviceConfig = {
        User = cfg.user;
        Group = cfg.group;
        UMask = "0077";
        WorkingDirectory = cfg.stateDir;
        EnvironmentFile = cfg.environmentFile;
        ExecStart = "${lib.getExe cfg.package} /etc/knot/config.toml";
        Restart = "on-failure";
        RestartSec = 5;
        TimeoutStopSec = 120;
        LimitNOFILE = 65536;
        AmbientCapabilities = lib.mkIf bindsPrivilegedPort ["CAP_NET_BIND_SERVICE"];
        CapabilityBoundingSet =
          if bindsPrivilegedPort
          then ["CAP_NET_BIND_SERVICE"]
          else [];
        ReadWritePaths = stateDirs ++ map (dir: "-${dir}") keyDirs;
        NoNewPrivileges = true;
        ProtectProc = "invisible";
        ProtectSystem = "strict";
        ProtectHome = !usesHomePath;
        PrivateTmp = true;
        PrivateDevices = true;
        PrivateUsers = !bindsPrivilegedPort;
        ProtectHostname = true;
        ProtectClock = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_NETLINK" "AF_UNIX"];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        PrivateMounts = true;
        SystemCallFilter = ["@system-service" "~@privileged @resources"];
        SystemCallArchitectures = "native";
      };
    };

    networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall publicTcpPorts;
    networking.firewall.allowedUDPPorts = lib.mkIf cfg.openFirewall publicUdpPorts;
  };
}
