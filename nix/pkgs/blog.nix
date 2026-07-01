{
  buildGoApplication,
  runCommandLocal,
  modules,
  appview-static-files,
  sqlite-lib,
  src,
}: let
  blog-bin = buildGoApplication {
    pname = "blog";
    version = "0.1.0";
    inherit src modules;

    postUnpack = ''
      pushd source
      mkdir -p appview/pages/static
      cp -frv ${appview-static-files}/* appview/pages/static
      popd
    '';

    doCheck = false;
    subPackages = ["cmd/blog"];

    tags = ["libsqlite3"];
    env.CGO_CFLAGS = "-I ${sqlite-lib}/include ";
    env.CGO_LDFLAGS = "-L ${sqlite-lib}/lib";
    CGO_ENABLED = 1;
  };
in
  runCommandLocal "blog" {
    TANGLED_AVATAR_SHARED_SECRET = builtins.getEnv "TANGLED_AVATAR_SHARED_SECRET";
  } ''
    mkdir -p working
    cp -r --no-preserve=mode ${src}/blog working/
    cp -r --no-preserve=mode ${src}/appview working/

    mkdir -p working/appview/pages/static
    cp -fr --no-preserve=mode ${appview-static-files}/* working/appview/pages/static/

    cd working
    ${blog-bin}/bin/blog build

    mkdir -p $out
    cp -r build/* $out/
  ''
