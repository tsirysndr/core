{
  nixpkgs,
  system,
  hostSystem,
  self,
}: let
  envVar = name: let
    var = builtins.getEnv name;
  in
    if var == ""
    then throw "\$${name} must be defined, see https://docs.tangled.org/hacking-on-tangled.html#hacking-on-tangled for more details"
    else var;
  envVarOr = name: default: let
    var = builtins.getEnv name;
  in
    if var != ""
    then var
    else default;

  plcUrl = envVarOr "TANGLED_VM_PLC_URL" "https://plc.directory";
  jetstream = envVarOr "TANGLED_VM_JETSTREAM_ENDPOINT" "wss://jetstream1.us-west.bsky.network/subscribe";
in
  nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      self.nixosModules.knot
      self.nixosModules.spindle
      self.nixosModules.knotmirror
      ({
        lib,
        config,
        pkgs,
        ...
      }: {
        virtualisation.vmVariant.virtualisation = {
          host.pkgs = import nixpkgs {system = hostSystem;};

          graphics = false;
          memorySize = 2048;
          diskSize = 10 * 1024;
          cores = 2;
          forwardPorts = [
            # ssh
            {
              from = "host";
              host.port = 2222;
              guest.port = 22;
            }
            # knot
            {
              from = "host";
              host.port = 6444;
              guest.port = 6444;
            }
            # spindle
            {
              from = "host";
              host.port = 6555;
              guest.port = 6555;
            }
            # knotmirror
            {
              from = "host";
              host.port = 7007; # 7000 is deserved in macos for Airplay
              guest.port = 7000;
            }
            # knotmirror-tap
            {
              from = "host";
              host.port = 7480;
              guest.port = 7480;
            }
            # knotmirror-admin
            {
              from = "host";
              host.port = 7200;
              guest.port = 7200;
            }
          ];
          sharedDirectories = {
            # We can't use the 9p mounts directly for most of these
            # as SQLite is incompatible with them. So instead we
            # mount the shared directories to a different location
            # and copy the contents around on service start/stop.
            knotData = {
              source = "$TANGLED_VM_DATA_DIR/knot";
              target = "/mnt/knot-data";
            };
            spindleData = {
              source = "$TANGLED_VM_DATA_DIR/spindle";
              target = "/mnt/spindle-data";
            };
            spindleLogs = {
              source = "$TANGLED_VM_DATA_DIR/spindle-logs";
              target = "/var/log/spindle";
            };
          };
        };
        # This is fine because any and all ports that are forwarded to host are explicitly marked above, we don't need a separate guest firewall
        networking.firewall.enable = false;
        time.timeZone = "Europe/London";
        services.getty.autologinUser = "root";
        environment.systemPackages = with pkgs; [curl vim git sqlite litecli postgresql_14];
        services.tangled.knot = {
          enable = true;
          motd = "Welcome to the development knot!\n";
          server = {
            owner = envVar "TANGLED_VM_KNOT_OWNER";
            hostname = envVarOr "TANGLED_VM_KNOT_HOST" "localhost:6444";
            plcUrl = plcUrl;
            jetstreamEndpoint = jetstream;
            listenAddr = "0.0.0.0:6444";
            dev = true;
          };
          knotmirrors = [
            "http://localhost:7000"
          ];
        };
        services.tangled.spindle = {
          enable = true;
          environmentFile = "/var/lib/spindle/.env";
          server = {
            owner = envVar "TANGLED_VM_SPINDLE_OWNER";
            hostname = envVarOr "TANGLED_VM_SPINDLE_HOST" "localhost:6555";
            plcUrl = plcUrl;
            jetstreamEndpoint = jetstream;
            listenAddr = "0.0.0.0:6555";
            dev = true;
            queueSize = 100;
            maxJobCount = 2;
            secrets = {
              provider = "sqlite";
            };
          };

          pipelines = {
            logBucket = envVarOr "SPINDLE_S3_LOG_BUCKET" "";
          };
        };
        services.postgresql = {
          enable = true;
          package = pkgs.postgresql_14;
          ensureDatabases = ["mirror" "tap"];
          ensureUsers = [
            {name = "tnglr";}
          ];
          authentication = ''
            local all tnglr              trust
            host  all tnglr 127.0.0.1/32 trust
          '';
        };
        services.tangled.knotmirror = {
          enable = true;
          listenAddr = "0.0.0.0:7000";
          adminListenAddr = "0.0.0.0:7200";
          hostname = "localhost:7000";
          dbUrl = "postgresql://tnglr@127.0.0.1:5432/mirror";
          fullNetwork = false;
          tap.dbUrl = "postgresql://tnglr@127.0.0.1:5432/tap";
        };
        users = {
          # So we don't have to deal with permission clashing between
          # blank disk VMs and existing state
          users.${config.services.tangled.knot.gitUser}.uid = 666;
          groups.${config.services.tangled.knot.gitUser}.gid = 666;

          # TODO: separate spindle user
        };
        systemd.services = let
          mkDataSyncScripts = source: target: {
            enableStrictShellChecks = true;

            preStart = lib.mkBefore ''
              mkdir -p ${target}
              ${lib.getExe pkgs.rsync} -a ${source}/ ${target}
            '';

            postStop = lib.mkAfter ''
              ${lib.getExe pkgs.rsync} -a ${target}/ ${source}
            '';

            serviceConfig.PermissionsStartOnly = true;
          };
        in {
          knot = mkDataSyncScripts "/mnt/knot-data" config.services.tangled.knot.stateDir;
          spindle = mkDataSyncScripts "/mnt/spindle-data" (builtins.dirOf config.services.tangled.spindle.server.dbPath);
          knotmirror.after = ["postgresql.target"];
          tap-knotmirror.after = ["postgresql.target"];
        };
      })
    ];
  }
