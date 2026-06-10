{
  lib,
  buildGoApplication,
  modules,
  writeShellScriptBin,
}: let
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../go.mod
      ../../ico
      ../../cmd/dolly/main.go
      ../../appview/pages/templates/fragments/dolly/logo.html
      ../../appview/pages/templates/fragments/dolly/logotype.html
    ];
  };
  dolly-unwrapped = buildGoApplication {
    pname = "dolly-unwrapped";
    version = "0.1.0";
    inherit src modules;
    doCheck = false;
    subPackages = ["cmd/dolly"];
  };
in
  writeShellScriptBin "dolly" ''
    exec ${dolly-unwrapped}/bin/dolly \
    -template ${src}/appview/pages/templates/fragments/dolly \
    "$@"
  ''
