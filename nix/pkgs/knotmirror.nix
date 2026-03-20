{
  buildGoApplication,
  modules,
  src,
}:
buildGoApplication {
  pname = "knotmirror";
  version = "0.1.0";
  inherit src modules;

  doCheck = false;

  subPackages = ["cmd/knotmirror"];

  meta = {
    mainProgram = "knotmirror";
  };
}
