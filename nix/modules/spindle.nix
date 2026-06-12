{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.tangled.spindle;
in
  with lib; {
    options = {
      services.tangled.spindle = {
        enable = mkOption {
          type = types.bool;
          default = false;
          description = "Enable a tangled spindle";
        };
        package = mkOption {
          type = types.package;
          description = "Package to use for the spindle";
        };

        server = {
          listenAddr = mkOption {
            type = types.str;
            default = "0.0.0.0:6555";
            description = "Address to listen on";
          };

          dbPath = mkOption {
            type = types.path;
            default = "/var/lib/spindle/spindle.db";
            description = "Path to the database file";
          };

          hostname = mkOption {
            type = types.str;
            example = "my.spindle.com";
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

          dev = mkOption {
            type = types.bool;
            default = false;
            description = "Enable development mode (disables signature verification)";
          };

          owner = mkOption {
            type = types.str;
            example = "did:plc:qfpnj4og54vl56wngdriaxug";
            description = "DID of owner (required)";
          };

          maxJobCount = mkOption {
            type = types.int;
            default = 2;
            example = 5;
            description = "Maximum number of concurrent jobs to run";
          };

          queueSize = mkOption {
            type = types.int;
            default = 100;
            example = 100;
            description = "Maximum number of jobs queue up";
          };

          secrets = {
            provider = mkOption {
              type = types.str;
              default = "sqlite";
              description = "Backend to use for secret management, valid options are 'sqlite', and 'openbao'.";
            };

            openbao = {
              proxyAddr = mkOption {
                type = types.str;
                default = "http://127.0.0.1:8200";
                description = "Address of the OpenBAO proxy server";
              };
              mount = mkOption {
                type = types.str;
                default = "spindle";
                description = "Mount path in OpenBAO to read secrets from";
              };
            };
          };

          tap = {
            embed = mkOption {
              type = types.bool;
              default = true;
              description = "Run an embedded tap inside the spindle process";
            };

            url = mkOption {
              type = types.str;
              default = "http://[::1]:2480";
              description = "URL the spindle's tap client dials";
            };

            bind = mkOption {
              type = types.str;
              default = "[::1]:2480";
              description = "Loopback address the embedded tap server listens on";
            };

            dbPath = mkOption {
              type = types.path;
              default = "/var/lib/spindle/tap.db";
              description = "Path to the embedded tap sqlite database";
            };

            relayUrl = mkOption {
              type = types.str;
              default = "https://bsky.network";
              description = "Relay used by the embedded tap firehose";
            };
          };
        };

        pipelines = {
          logBucket = mkOption {
            type = types.str;
            default = "tangled-logs";
            description = "S3 bucket for workflow logs";
          };
          workflowTimeout = mkOption {
            type = types.str;
            default = "5m";
            description = "Timeout for each workflow step";
          };

          nixery = {
            nixery = mkOption {
              type = types.str;
              default = "nixery.tangled.sh"; # note: this is *not* on tangled.org yet
              description = "Nixery instance to use";
            };

            maxJobMemoryMb = mkOption {
              type = types.int;
              default = 6144;
              description = "Memory limit per nixery workflow container in MiB (default 6 GiB)";
            };
            maxConcurrentWorkflows = mkOption {
              type = types.int;
              default = 8;
              description = "Maximum number of nixery workflows running simultaneously. Zero disables this limit.";
            };
          };

          microvm = {
            enableKVM = mkOption {
              type = types.bool;
              default = true;
              description = "Enable KVM hardware acceleration";
            };

            imageDir = mkOption {
              type = types.str;
              default = "/var/lib/spindle/images";
              description = "Directory containing microVM image spec JSONs or image spec directories";
            };
            overlayDir = mkOption {
              type = types.str;
              default = "/tmp";
              description = "Directory to store microVM temporary overlay files";
            };
            defaultImage = mkOption {
              type = types.str;
              default = "nixos";
              description = "Default microVM image spec to use if none is specified in workflow";
            };
            agentPort = mkOption {
              type = types.port;
              default = 10240;
              description = "Host vsock port the microVM agent connects back to";
            };

            limits = {
              total = {
                memoryMiB = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared guest memory in MiB allowed across all running microVM workflows. Zero disables this limit.";
                };
                vcpus = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared vCPUs allowed across all running microVM workflows. Zero disables this limit.";
                };
                diskMiB = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared disk in MiB allowed across all running microVM workflows. Zero disables this limit.";
                };
              };

              workflow = {
                memoryMiB = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared guest memory in MiB allowed for a single microVM workflow. Zero disables this limit.";
                };
                vcpus = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared vCPUs allowed for a single microVM workflow. Zero disables this limit.";
                };
                diskMiB = mkOption {
                  type = types.int;
                  default = 0;
                  description = "Maximum declared disk in MiB allowed for a single microVM workflow. Zero disables this limit.";
                };
              };
            };

            cgroup = {
              enable = mkOption {
                type = types.bool;
                default = false;
                description = "Enable cgroup v2 containment for microVM processes.";
              };
              parent = mkOption {
                type = types.str;
                default = "self";
                description = "Parent cgroup for microVM workflow cgroups. Use 'self' to resolve the spindle service cgroup.";
              };
              pidsMax = mkOption {
                type = types.int;
                default = 4096;
                description = "Maximum number of processes allowed in each microVM workflow cgroup.";
              };
              swapMaxMiB = mkOption {
                type = types.int;
                default = 0;
                description = "Maximum swap in MiB allowed in each microVM workflow cgroup. Zero disables swap.";
              };
              supervisorMinMiB = mkOption {
                type = types.int;
                default = 512;
                description = ''
                  Amount of memory in MiB that will be protected by the cgroup for the spindle
                  (allowing it to not get OOMed first.)
                '';
              };
            };
          };
        };

        cache = {
          readUrls = mkOption {
            type = types.listOf types.str;
            default = [];
            example = ["http://ncps.internal:8501"];
            description = "Nix binary cache URLs the Spindle guest should read from.";
          };

          trustedPublicKeys = mkOption {
            type = types.listOf types.str;
            default = [];
            example = ["ncps.internal-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="];
            description = "Public keys trusted for the configured Nix binary caches.";
          };

          uploadUrl = mkOption {
            type = types.str;
            default = "";
            example = "http://ncps.internal:8501/upload";
            description = "Optional cache upload URL used by live cache import paths.";
          };
        };

        environmentFile = mkOption {
          type = with types; nullOr path;
          default = null;
          example = "/etc/spindle.env";
          description = ''
            Additional environment file as defined in {manpage}`systemd.exec(5)`.

            Sensitive secrets such as {env}`AWS_SECRET_ACCESS_KEY`,
            {env}`AWS_ACCESS_KEY_ID`, {env}`AWS_REGION`
            may be passed to the service
            without making them world readable in the nix store.
          '';
        };
      };
    };

    config = let
      deps = [
        pkgs.qemu
        pkgs.e2fsprogs
        pkgs.slirp4netns
        pkgs.iproute2
        pkgs.util-linux
      ];
    in
      mkIf cfg.enable {
        environment.systemPackages = [
          (pkgs.writeShellScriptBin "spindle" ''
            export PATH="${lib.makeBinPath deps}:$PATH"
            ${lib.optionalString (cfg.environmentFile != null) "set -a; source ${cfg.environmentFile}; set +a"}
            ${lib.concatMapStringsSep "\n" (
                e: "export ${e}"
              )
              config.systemd.services.spindle.serviceConfig.Environment}
            exec ${cfg.package}/bin/spindle "$@"
          '')
        ];

        virtualisation.docker.enable = true;

        systemd.services.spindle = {
          description = "spindle service";
          after = [
            "network.target"
            "docker.service"
          ];
          wantedBy = ["multi-user.target"];
          path = deps;
          serviceConfig = {
            LogsDirectory = "spindle";
            StateDirectory = "spindle";
            Delegate = cfg.pipelines.microvm.cgroup.enable;
            EnvironmentFile = mkIf (cfg.environmentFile != null) cfg.environmentFile;

            Environment = [
              "SPINDLE_SERVER_LISTEN_ADDR=${cfg.server.listenAddr}"
              "SPINDLE_SERVER_DB_PATH=${cfg.server.dbPath}"
              "SPINDLE_SERVER_HOSTNAME=${cfg.server.hostname}"
              "SPINDLE_SERVER_PLC_URL=${cfg.server.plcUrl}"
              "SPINDLE_SERVER_JETSTREAM_ENDPOINT=${cfg.server.jetstreamEndpoint}"
              "SPINDLE_SERVER_DEV=${lib.boolToString cfg.server.dev}"
              "SPINDLE_SERVER_OWNER=${cfg.server.owner}"
              "SPINDLE_SERVER_MAX_JOB_COUNT=${toString cfg.server.maxJobCount}"
              "SPINDLE_SERVER_QUEUE_SIZE=${toString cfg.server.queueSize}"
              "SPINDLE_SERVER_SECRETS_PROVIDER=${cfg.server.secrets.provider}"
              "SPINDLE_SERVER_SECRETS_OPENBAO_PROXY_ADDR=${cfg.server.secrets.openbao.proxyAddr}"
              "SPINDLE_SERVER_SECRETS_OPENBAO_MOUNT=${cfg.server.secrets.openbao.mount}"
              "SPINDLE_SERVER_TAP_EMBED=${lib.boolToString cfg.server.tap.embed}"
              "SPINDLE_SERVER_TAP_URL=${cfg.server.tap.url}"
              "SPINDLE_SERVER_TAP_BIND=${cfg.server.tap.bind}"
              "SPINDLE_SERVER_TAP_DB_PATH=${cfg.server.tap.dbPath}"
              "SPINDLE_SERVER_TAP_RELAY_URL=${cfg.server.tap.relayUrl}"
              "SPINDLE_NIXERY_PIPELINES_NIXERY=${cfg.pipelines.nixery.nixery}"
              "SPINDLE_NIXERY_PIPELINES_WORKFLOW_TIMEOUT=${cfg.pipelines.workflowTimeout}"
              "SPINDLE_NIXERY_PIPELINES_MAX_JOB_MEMORY_MB=${toString cfg.pipelines.nixery.maxJobMemoryMb}"
              "SPINDLE_NIXERY_PIPELINES_MAX_CONCURRENT_WORKFLOWS=${toString cfg.pipelines.nixery.maxConcurrentWorkflows}"
              "SPINDLE_MICROVM_PIPELINES_IMAGE_DIR=${cfg.pipelines.microvm.imageDir}"
              "SPINDLE_MICROVM_PIPELINES_OVERLAY_DIR=${cfg.pipelines.microvm.overlayDir}"
              "SPINDLE_MICROVM_PIPELINES_DEFAULT_IMAGE=${cfg.pipelines.microvm.defaultImage}"
              "SPINDLE_MICROVM_PIPELINES_AGENT_PORT=${toString cfg.pipelines.microvm.agentPort}"
              "SPINDLE_MICROVM_PIPELINES_ENABLE_KVM=${lib.boolToString cfg.pipelines.microvm.enableKVM}"
              "SPINDLE_MICROVM_PIPELINES_WORKFLOW_TIMEOUT=${cfg.pipelines.workflowTimeout}"
              "SPINDLE_MICROVM_PIPELINES_MAX_TOTAL_MEMORY_MIB=${toString cfg.pipelines.microvm.limits.total.memoryMiB}"
              "SPINDLE_MICROVM_PIPELINES_MAX_TOTAL_VCPUS=${toString cfg.pipelines.microvm.limits.total.vcpus}"
              "SPINDLE_MICROVM_PIPELINES_MAX_TOTAL_DISK_MIB=${toString cfg.pipelines.microvm.limits.total.diskMiB}"
              "SPINDLE_MICROVM_PIPELINES_MAX_WORKFLOW_MEMORY_MIB=${toString cfg.pipelines.microvm.limits.workflow.memoryMiB}"
              "SPINDLE_MICROVM_PIPELINES_MAX_WORKFLOW_VCPUS=${toString cfg.pipelines.microvm.limits.workflow.vcpus}"
              "SPINDLE_MICROVM_PIPELINES_MAX_WORKFLOW_DISK_MIB=${toString cfg.pipelines.microvm.limits.workflow.diskMiB}"
              "SPINDLE_MICROVM_PIPELINES_ENABLE_CGROUPS=${lib.boolToString cfg.pipelines.microvm.cgroup.enable}"
              "SPINDLE_MICROVM_PIPELINES_CGROUP_PARENT=${cfg.pipelines.microvm.cgroup.parent}"
              "SPINDLE_MICROVM_PIPELINES_CGROUP_PIDS_MAX=${toString cfg.pipelines.microvm.cgroup.pidsMax}"
              "SPINDLE_MICROVM_PIPELINES_CGROUP_SWAP_MAX_MIB=${toString cfg.pipelines.microvm.cgroup.swapMaxMiB}"
              "SPINDLE_MICROVM_PIPELINES_CGROUP_SUPERVISOR_MEMORY_MIN_MIB=${toString cfg.pipelines.microvm.cgroup.supervisorMinMiB}"
              "SPINDLE_NIX_CACHE_READ_URLS=${concatStringsSep "," cfg.cache.readUrls}"
              "SPINDLE_NIX_CACHE_TRUSTED_PUBLIC_KEYS=${concatStringsSep "," cfg.cache.trustedPublicKeys}"
              "SPINDLE_NIX_CACHE_UPLOAD_URL=${cfg.cache.uploadUrl}"
              "SPINDLE_S3_LOG_BUCKET=${cfg.pipelines.logBucket}"
            ];
            ExecStart = "${cfg.package}/bin/spindle";
            Restart = "always";
          };
        };
      };
  }
