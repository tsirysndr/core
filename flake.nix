{
  description = "atproto github";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    gomod2nix = {
      url = "github:nix-community/gomod2nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-compat = {
      url = "https://git.lix.systems/lix-project/flake-compat/archive/main.tar.gz";
      flake = false;
    };
    indigo = {
      url = "github:oppiliappan/indigo";
      flake = false;
    };
    htmx-src = {
      url = "https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js";
      flake = false;
    };
    htmx-ws-src = {
      # strange errors in console that i can't really make out
      # url = "https://unpkg.com/htmx.org@2.0.4/dist/ext/ws.js";
      url = "https://cdn.jsdelivr.net/npm/htmx-ext-ws@2.0.2";
      flake = false;
    };
    lucide-src = {
      url = "https://github.com/lucide-icons/lucide/releases/download/0.536.0/lucide-icons-0.536.0.zip";
      flake = false;
    };
    inter-fonts-src = {
      url = "https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip";
      flake = false;
    };
    actor-typeahead-src = {
      url = "git+https://tangled.org/@jakelazaroff.com/actor-typeahead";
      flake = false;
    };
    mermaid-src = {
      url = "https://cdn.jsdelivr.net/npm/mermaid@11.12.3/dist/mermaid.min.js";
      flake = false;
    };
    hls-src = {
      url = "https://cdn.jsdelivr.net/npm/hls.js@1.5.13/dist/hls.min.js";
      flake = false;
    };
    mathjax-src = {
      url = "https://cdn.jsdelivr.net/npm/mathjax@4.1.2/tex-svg.js";
      flake = false;
    };
    ibm-plex-mono-src = {
      url = "https://github.com/IBM/plex/releases/download/%40ibm%2Fplex-mono%401.1.0/ibm-plex-mono.zip";
      flake = false;
    };
    sqlite-lib-src = {
      url = "https://sqlite.org/2024/sqlite-amalgamation-3450100.zip";
      flake = false;
    };
    fetch-tangled = {
      url = "git+https://tangled.org/isabelroses.com/fetch-tangled";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    gomod2nix,
    indigo,
    htmx-src,
    htmx-ws-src,
    lucide-src,
    inter-fonts-src,
    sqlite-lib-src,
    ibm-plex-mono-src,
    actor-typeahead-src,
    mermaid-src,
    hls-src,
    microvm,
    fetch-tangled,
    mathjax-src,
    ...
  }: let
    supportedSystems = ["x86_64-linux" "x86_64-darwin" "aarch64-linux" "aarch64-darwin"];
    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    nixpkgsFor = forAllSystems (system:
      import nixpkgs {
        inherit system;
        overlays = [fetch-tangled.overlays.default];
      });

    mkPackageSet = pkgs:
      pkgs.lib.makeScope pkgs.newScope (self: {
        src = let
          fs = pkgs.lib.fileset;
        in
          fs.toSource {
            root = ./.;
            fileset = fs.difference (fs.intersection (fs.gitTracked ./.) (fs.fileFilter (file: !(file.hasExt "nix")) ./.)) (fs.maybeMissing ./.jj);
          };
        rustSrc = let
          fs = pkgs.lib.fileset;
        in
          fs.toSource {
            root = ./.;
            fileset =
              fs.intersection
              (fs.fromSource self.src)
              (fs.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./shuttle
                ./bobbin
              ]);
          };
        buildGoApplication =
          (self.callPackage "${gomod2nix}/builder" {
            gomod2nix = gomod2nix.legacyPackages.${pkgs.stdenv.hostPlatform.system}.gomod2nix;
          }).buildGoApplication;
        rustPlatform = pkgs.makeRustPlatform {
          inherit (fenix.packages.${pkgs.stdenv.hostPlatform.system}.stable) rustc cargo;
        };
        rustPlatformStatic = let
          system = pkgs.stdenv.hostPlatform.system;
          muslTarget = pkgs.pkgsStatic.stdenv.hostPlatform.rust.rustcTarget;
          toolchain = fenix.packages.${system}.combine [
            fenix.packages.${system}.stable.cargo
            fenix.packages.${system}.stable.rustc
            fenix.packages.${system}.targets.${muslTarget}.stable.rust-std
          ];
        in
          pkgs.pkgsStatic.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        modules = ./nix/gomod2nix.toml;
        sqlite-lib = self.callPackage ./nix/pkgs/sqlite-lib.nix {
          inherit sqlite-lib-src;
        };
        lexgen = self.callPackage ./nix/pkgs/lexgen.nix {inherit indigo;};
        goat = self.callPackage ./nix/pkgs/goat.nix {inherit indigo;};
        appview-static-files = self.callPackage ./nix/pkgs/appview-static-files.nix {
          inherit htmx-src htmx-ws-src lucide-src inter-fonts-src ibm-plex-mono-src actor-typeahead-src mermaid-src hls-src mathjax-src;
        };
        appview = self.callPackage ./nix/pkgs/appview.nix {};
        blog = self.callPackage ./nix/pkgs/blog.nix {};
        docs = self.callPackage ./nix/pkgs/docs.nix {
          inherit inter-fonts-src ibm-plex-mono-src lucide-src;
          inherit (pkgs) pagefind;
        };
        spindle = self.callPackage ./nix/pkgs/spindle.nix {};
        shuttle = self.callPackage ./nix/pkgs/shuttle.nix {
          src = self.rustSrc;
        };
        shuttle-static = self.callPackage ./nix/pkgs/shuttle.nix {
          src = self.rustSrc;
          rustPlatform = self.rustPlatformStatic;
        };
        knot-unwrapped = self.callPackage ./nix/pkgs/knot-unwrapped.nix {};
        knot = self.callPackage ./nix/pkgs/knot.nix {};
        dolly = self.callPackage ./nix/pkgs/dolly.nix {};
        tap = self.callPackage ./nix/pkgs/tap.nix {};
        knotmirror = self.callPackage ./nix/pkgs/knotmirror.nix {};
        bobbin = self.callPackage ./nix/pkgs/bobbin.nix {};
        zoekt-webserver = self.callPackage ./nix/pkgs/zoekt-webserver.nix {};
        zoekt-tngl-indexserver = self.callPackage ./nix/pkgs/zoekt-tngl-indexserver.nix {};
      });
  in {
    overlays.default = final: prev: {
      inherit
        (mkPackageSet final)
        lexgen
        goat
        sqlite-lib
        spindle
        shuttle
        knot-unwrapped
        knot
        appview
        docs
        dolly
        tap
        knotmirror
        bobbin
        zoekt-webserver
        zoekt-tngl-indexserver
        ;
    };

    packages = forAllSystems (system: let
      pkgs = nixpkgsFor.${system};
      linuxPkgs = nixpkgsFor."x86_64-linux";
      packages = mkPackageSet pkgs;
      staticPackages = mkPackageSet pkgs.pkgsStatic;
      crossPackages = mkPackageSet pkgs.pkgsCross.gnu64.pkgsStatic;
    in {
      inherit
        (packages)
        appview
        appview-static-files
        blog
        lexgen
        goat
        spindle
        knot
        knot-unwrapped
        sqlite-lib
        docs
        shuttle
        shuttle-static
        dolly
        tap
        knotmirror
        bobbin
        zoekt-webserver
        zoekt-tngl-indexserver
        ;

      pkgsStatic-appview = staticPackages.appview;
      pkgsStatic-knot = staticPackages.knot;
      pkgsStatic-knot-unwrapped = staticPackages.knot-unwrapped;
      pkgsStatic-spindle = staticPackages.spindle;
      pkgsStatic-sqlite-lib = staticPackages.sqlite-lib;
      pkgsStatic-dolly = staticPackages.dolly;

      pkgsCross-gnu64-pkgsStatic-appview = crossPackages.appview;
      pkgsCross-gnu64-pkgsStatic-knot = crossPackages.knot;
      pkgsCross-gnu64-pkgsStatic-knot-unwrapped = crossPackages.knot-unwrapped;
      pkgsCross-gnu64-pkgsStatic-spindle = crossPackages.spindle;
      pkgsCross-gnu64-pkgsStatic-dolly = crossPackages.dolly;

      treefmt-wrapper = pkgs.treefmt.withConfig {
        settings.formatter = {
          alejandra = {
            command = pkgs.lib.getExe pkgs.alejandra;
            includes = ["*.nix"];
          };

          gofmt = {
            command = pkgs.lib.getExe' pkgs.go "gofmt";
            options = ["-w"];
            includes = ["*.go"];
          };

          rustfmt = {
            command = pkgs.lib.getExe' fenix.packages.${system}.stable.rustfmt "rustfmt";
            options = ["--edition" "2024"];
            includes = ["*.rs"];
            excludes = ["**/src/_lex/**"];
          };

          # prettier = let
          #   wrapper = pkgs.runCommandLocal "prettier-wrapper" {nativeBuildInputs = [pkgs.makeWrapper];} ''
          #     makeWrapper ${pkgs.prettier}/bin/prettier "$out" --add-flags "--plugin=${pkgs.prettier-plugin-go-template}/lib/node_modules/prettier-plugin-go-template/lib/index.js"
          #   '';
          # in {
          #   command = wrapper;
          #   options = ["-w"];
          #   includes = ["*.html"];
          #   # causes Go template plugin errors: https://github.com/NiklasPor/prettier-plugin-go-template/issues/120
          #   excludes = ["appview/pages/templates/layouts/repobase.html" "appview/pages/templates/repo/tags.html"];
          # };
        };
      };

      spindle-nixos-image = linuxPkgs.callPackage ./nix/pkgs/spindle-nixos-image.nix {
        nixosSystem = self.nixosConfigurations.spindle-nixos;
      };
      spindle-nixos-image-tarball = linuxPkgs.runCommand "spindle-nixos-image-tarball.tar.gz" {} ''
        tar -S -C ${self.packages.${system}.spindle-nixos-image} -h -czf $out .
      '';

      spindle-alpine-image = let
        branch = "3.24";
        version = "${branch}.0";
        arch = "x86_64";
        cdn = "https://dl-cdn.alpinelinux.org/alpine/v${branch}/releases/${arch}";

        shuttle = (mkPackageSet linuxPkgs).shuttle-static;
      in
        linuxPkgs.callPackage ./nix/pkgs/spindle-alpine-image.nix {
          inherit arch shuttle;
          repositories = [
            "https://dl-cdn.alpinelinux.org/alpine/v${branch}/main"
            "https://dl-cdn.alpinelinux.org/alpine/v${branch}/community"
          ];
          rootfs = linuxPkgs.fetchurl {
            url = "${cdn}/alpine-minirootfs-${version}-${arch}.tar.gz";
            hash = "sha256-3poRwODn6clNs+2K97RQ6vwLE2h71+kZnVUFDyCqCok=";
          };
          kernel = linuxPkgs.fetchurl {
            url = "${cdn}/netboot-${version}/vmlinuz-virt";
            hash = "sha256-Hmv5Ancgx1w+0NeRcfIbV5HuQMqXldB8fG4E3F6irpA=";
          };
          initramfs = linuxPkgs.fetchurl {
            url = "${cdn}/netboot-${version}/initramfs-virt";
            hash = "sha256-ZCWGSaVMOYOmLz1Gwsf2RhYarMqk+tFVA6MMDWiHVJQ=";
          };
          modloop = linuxPkgs.fetchurl {
            url = "${cdn}/netboot-${version}/modloop-virt";
            hash = "sha256-p3yO7yU28k04iT01sOzhDmEYi+Yl7VZs5r3RYsWCBX0=";
          };
        };
      spindle-alpine-image-tarball = linuxPkgs.runCommand "spindle-alpine-image-tarball.tar.gz" {} ''
        tar -S -C ${self.packages.${system}.spindle-alpine-image} -h -czf $out .
      '';
    });
    defaultPackage = forAllSystems (system: self.packages.${system}.appview);
    devShells = forAllSystems (system: let
      pkgs = nixpkgsFor.${system};
      packages' = self.packages.${system};
      staticShell = args:
        (pkgs.mkShell.override {
          stdenv = pkgs.pkgsStatic.stdenv;
        }) (args
          // {
            nativeBuildInputs =
              args.nativeBuildInputs
              ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
                pkgs.darwin.cctools
              ];
          });
    in {
      default = staticShell {
        nativeBuildInputs =
          [
            pkgs.go
            pkgs.air
            pkgs.gopls
            pkgs.httpie
            pkgs.litecli
            pkgs.websocat
            pkgs.tailwindcss
            pkgs.nixos-shell
            pkgs.redis
            pkgs.worker-build
            pkgs.cargo-generate
            pkgs.qemu
            pkgs.cdrkit
            pkgs.buf
            pkgs.protobuf
            pkgs.protoc-gen-prost
            pkgs.protoc-gen-prost-crate
            pkgs.protoc-gen-prost-serde
            pkgs.protoc-gen-go
            (fenix.packages.${system}.combine [
              fenix.packages.${system}.stable.cargo
              fenix.packages.${system}.stable.rustc
              fenix.packages.${system}.stable.rust-src
              fenix.packages.${system}.stable.clippy
              fenix.packages.${system}.stable.rustfmt
              fenix.packages.${system}.targets.wasm32-unknown-unknown.stable.rust-std
            ])
            pkgs.coreutils # for those of us who are on systems that use busybox (alpine)
            packages'.lexgen
            packages'.treefmt-wrapper
            packages'.tap
            pkgs.e2fsprogs
            pkgs.util-linux
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.parted
            pkgs.slirp4netns
            pkgs.iproute2
          ];
        shellHook = ''
          mkdir -p appview/pages/static
          # temporary self-heal for workspaces that copied static assets as read-only
          [ -d appview/pages/static/icons ] && [ ! -w appview/pages/static/icons ] && chmod -R u+rwX appview/pages/static
          # no preserve is needed because watch-tailwind will want to be able to overwrite
          cp -fr --no-preserve=ownership,mode ${packages'.appview-static-files}/* appview/pages/static
          export TANGLED_OAUTH_CLIENT_KID="$(date +%s)"
          export TANGLED_OAUTH_CLIENT_SECRET="$(${packages'.goat}/bin/goat key generate -t P-256 | grep -A1 "Secret Key" | tail -n1 | awk '{print $1}')"
          # Make xcrun (Nix stub) able to find ld from the system Command Line Tools.
          # Without this, worker-build/cargo fails with "error: tool 'ld' not found".
          if [ -d /Library/Developer/CommandLineTools/usr/bin ]; then
            export PATH=/Library/Developer/CommandLineTools/usr/bin:$PATH
          fi
        '';
        env.CGO_ENABLED = 1;
      };
    });
    apps = forAllSystems (system: let
      pkgs = nixpkgsFor."${system}";
      packages' = self.packages.${system};
      air-watcher = name: arg:
        pkgs.writeShellScriptBin "run"
        ''
          export PATH=${pkgs.go}/bin:$PATH
          ${pkgs.air}/bin/air -c ./.air/${name}.toml \
            -build.args_bin "${arg}"
        '';
      tailwind-watcher =
        pkgs.writeShellScriptBin "run"
        ''
          export BROWSERSLIST_IGNORE_OLD_DATA=true
          ${pkgs.tailwindcss}/bin/tailwindcss --watch=always -i input.css -o ./appview/pages/static/tw.css
        '';
    in {
      watch-appview = {
        type = "app";
        program = toString (pkgs.writeShellScript "watch-appview" ''
          echo "copying static files to appview/pages/static..."
          mkdir -p appview/pages/static
          ${pkgs.coreutils}/bin/cp -fr --no-preserve=ownership,mode ${packages'.appview-static-files}/* appview/pages/static
          ${air-watcher "appview" ""}/bin/run
        '');
      };
      watch-knot = {
        type = "app";
        program = ''${air-watcher "knot" "server"}/bin/run'';
      };
      watch-spindle = {
        type = "app";
        program = ''${air-watcher "spindle" ""}/bin/run'';
      };
      watch-blog = {
        type = "app";
        program = toString (pkgs.writeShellScript "watch-blog" ''
          echo "copying static files to appview/pages/static..."
          mkdir -p appview/pages/static
          ${pkgs.coreutils}/bin/cp -fr --no-preserve=ownership,mode ${packages'.appview-static-files}/* appview/pages/static
          ${air-watcher "blog" "serve"}/bin/run
        '');
      };
      watch-tailwind = {
        type = "app";
        program = ''${tailwind-watcher}/bin/run'';
      };
      serve-docs = {
        type = "app";
        program = toString (pkgs.writeShellScript "serve-docs" ''
          echo "building docs..."
          docsOut=$(nix build --no-link --print-out-paths .#docs)
          echo "serving docs at http://localhost:1414"
          cd "$docsOut"
          exec ${pkgs.python3}/bin/python3 -m http.server 1414
        '');
      };
      regenerate-proto = {
        type = "app";
        program =
          (pkgs.writeShellApplication {
            name = "regenerate-proto";
            runtimeInputs = with pkgs; [git buf coreutils];
            text = ''
              rootDir=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
              cd "$rootDir"
              echo ">>> regenerating protobuf files.."
              buf generate
              echo ">>> generating file descriptor set for shuttle..."
              buf build -o shuttle/src/gen/file_descriptor_set.bin
              echo ">>> done"
            '';
          })
          + "/bin/regenerate-proto";
      };
      vm = let
        guestSystem =
          if pkgs.stdenv.hostPlatform.isAarch64
          then "aarch64-linux"
          else "x86_64-linux";
      in {
        type = "app";
        program =
          (pkgs.writeShellApplication {
            name = "launch-vm";
            text = ''
              rootDir=$(jj --ignore-working-copy root || git rev-parse --show-toplevel) || (echo "error: can't find repo root?"; exit 1)
              cd "$rootDir"

              # Reset TMPDIR in case it points to a stale nix-shell temp dir
              export TMPDIR=/tmp

              mkdir -p nix/vm-data/{knot,repos,spindle,spindle-logs}

              export TANGLED_VM_DATA_DIR="$rootDir/nix/vm-data"
              exec ${pkgs.lib.getExe
                (import ./nix/vm.nix {
                  inherit nixpkgs self;
                  system = guestSystem;
                  hostSystem = system;
                }).config.system.build.vm}
            '';
          })
          + /bin/launch-vm;
      };
      gomod2nix = {
        type = "app";
        program = toString (pkgs.writeShellScript "gomod2nix" ''
          ${gomod2nix.legacyPackages.${system}.gomod2nix}/bin/gomod2nix generate --outdir ./nix
        '');
      };
      lexgen = {
        type = "app";
        program =
          (pkgs.writeShellApplication {
            name = "lexgen";
            text = ''
              if ! command -v lexgen > /dev/null; then
                echo "error: must be executed from devshell"
                exit 1
              fi

              rootDir=$(jj --ignore-working-copy root || git rev-parse --show-toplevel) || (echo "error: can't find repo root?"; exit 1)
              cd "$rootDir"

              # *_ext.go are hand-written extensions; never remove or mutate them
              find api/tangled -maxdepth 1 -type f -not -name '*_ext.go' -delete
              lexgen --build-file lexicon-build-config.json lexicons

              # disable type registration temporarily while running cborgen
              find api/tangled -maxdepth 1 -name '*.go' -not -name '*_ext.go' -exec \
                sed -i.bak 's/\tutil/\/\/\tutil/' {} +
              # lexgen generates incomplete Marshaler/Unmarshaler for union types
              find api/tangled/*.go -not -name "cbor_gen.go" -exec \
                sed -i '/^func.*\(MarshalCBOR\|UnmarshalCBOR\)/,/^}/ s/^/\/\/ /' {} +
              for f in api/tangled/*_ext.go; do [ -e "''$f" ] && mv "''$f" "''$f.bak"; done
              ${pkgs.gotools}/bin/goimports -w api/tangled/*
              CGO_ENABLED=0 go run ./cmd/cborgen/
              for f in api/tangled/*_ext.go.bak; do [ -e "''$f" ] && mv "''$f" "''${f%.bak}"; done

              lexgen --build-file lexicon-build-config.json lexicons
              rm api/tangled/*.bak
            '';
          })
          + /bin/lexgen;
      };
    });

    nixosModules.appview = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/appview.nix];

      services.tangled.appview.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.appview;
    };
    nixosModules.knotmirror = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/knotmirror.nix];

      services.tangled.knotmirror.tap-package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.tap;
      services.tangled.knotmirror.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.knotmirror;
    };
    nixosModules.zoekt-tnglserver = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/zoekt-tnglserver.nix];

      services.tangled.zoekt.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.zoekt-tngl-indexserver;
      services.tangled.zoekt.zoekt-webserver-package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.zoekt-webserver;
    };
    nixosModules.knot = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/knot.nix];

      services.tangled.knot.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.knot;
    };
    nixosModules.spindle = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/spindle.nix];

      services.tangled.spindle.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.spindle;
    };
    nixosModules.shuttle = {
      lib,
      pkgs,
      ...
    }: {
      imports = [./nix/modules/shuttle.nix];

      services.tangled.shuttle.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.shuttle;
    };

    nixosModules.spindle-nixos = import ./nix/microvm/spindle-vm.nix {inherit self microvm;} ./nix/microvm/qemu.nix;

    formatter = forAllSystems (system: self.packages.${system}.treefmt-wrapper);

    nixosConfigurations = let
      spindleNixosBase = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [self.nixosModules.spindle-nixos];
      };
    in {
      spindle-nixos = spindleNixosBase;
    };
  };
}
