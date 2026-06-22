{
  pkgs,
  lib,
  options,
  ...
} @ args: let
  configPath = /run/spindle/user-config/config.json;
  userConfig =
    args.userConfig
    or (
      if builtins.pathExists configPath
      then lib.importJSON configPath
      else {}
    );

  registry = userConfig.registry or {};

  # registry targets may be structured attrs or flake ref strings; strings are
  # parsed by nix itself in getFlake. flakeRefToString rejects unforced attr
  # values, hence the toJSON round-trip
  toRefString = target:
    if builtins.isAttrs target
    then builtins.flakeRefToString (builtins.fromJSON (builtins.toJSON target))
    else target;

  # user registry entries shadow the system registry (which pins nixpkgs)
  getFlake = ref: builtins.getFlake (toRefString (registry.${ref} or ref));

  # "flakeref#attr" or a bare attr looked up in nixpkgs. nixpkgs refs use the
  # already-evaluated pkgs directly instead of re-evaluating via getFlake,
  # unless the user remapped nixpkgs in their registry
  resolvePackage = ref: let
    parts = lib.splitString "#" ref;
    hasAttr = lib.length parts > 1;
    flakeRef =
      if hasAttr
      then lib.head parts
      else "nixpkgs";
    pkgName =
      if hasAttr
      then lib.elemAt parts 1
      else ref;
    system = pkgs.stdenv.hostPlatform.system;
    flake = getFlake flakeRef;
    notFound = throw "Package ${pkgName} not found in ${flakeRef}";
  in
    if flakeRef == "nixpkgs" && !(registry ? nixpkgs)
    then pkgs.${pkgName} or notFound
    else flake.legacyPackages.${system}.${pkgName} or flake.packages.${system}.${pkgName} or notFound;

  # strings are resolved as package references only where the option type
  # actually expects packages; everything else passes through untouched
  resolveForType = type: v:
    if type.name == "package" && builtins.isString v
    then resolvePackage v
    # path-typed options (e.g. services.udev.packages) accept derivations via
    # coercion; "#" disambiguates flake refs from actual paths, which are
    # always absolute
    else if type.name == "path" && builtins.isString v && lib.hasInfix "#" v && !lib.hasPrefix "/" v
    then resolvePackage v
    else if type.name == "nullOr" && v != null
    then resolveForType type.nestedTypes.elemType v
    else if type.name == "listOf" && builtins.isList v
    then map (resolveForType type.nestedTypes.elemType) v
    else if (type.name == "attrsOf" || type.name == "lazyAttrsOf") && builtins.isAttrs v
    then builtins.mapAttrs (_: resolveForType type.nestedTypes.elemType) v
    else if type.name == "submodule" && builtins.isAttrs v
    then resolveOptions (type.getSubOptions []) v
    else v;

  resolveOptions = opts: builtins.mapAttrs (name: resolveValue (opts.${name} or null));

  resolveValue = opt: v:
    if !builtins.isAttrs opt
    then v
    else if lib.isOption opt
    then resolveForType opt.type v
    else if builtins.isAttrs v
    then resolveOptions opt v
    else v;

  # `foo = true` is shorthand for `foo.enable = true`, but only when an
  # enable option actually exists under foo
  hasEnableOption = opt:
    builtins.isAttrs opt
    && (
      if lib.isOption opt
      then (opt.type.getSubOptions opt.loc) ? enable
      else opt ? enable && lib.isOption opt.enable
    );

  normalize = opts: name: v: let
    opt = opts.${name} or null;
  in
    if builtins.isBool v && hasEnableOption opt
    then {enable = v;}
    else resolveValue opt v;

  # dependencies go into a devshell so we can make use of stdenv setup hooks
  # (e.g. for pkg-config and such)
  dependencies = userConfig.dependencies or [];
  spindleDevShell = pkgs.mkShellNoCC {
    name = "spindle-deps";
    packages = map resolvePackage dependencies;
  };
in {
  nix.registry = builtins.mapAttrs (name: _:
    lib.mkForce {
      to = {
        type = "path";
        path = (getFlake name).outPath;
      };
    })
  registry;
  # put the devshell into the resulting image env.
  # we do this instead of using a `.nix` file because it lets us skip eval time.
  environment.etc = lib.mkIf (dependencies != []) {
    "spindle/devshell.drv".source = spindleDevShell.drvPath;
    "spindle/devshell-inputs".source = spindleDevShell.inputDerivation;
  };
  services = builtins.mapAttrs (normalize (options.services or {})) (userConfig.services or {});
  virtualisation = builtins.mapAttrs (normalize (options.virtualisation or {})) (userConfig.virtualisation or {});
}
