{
  buildGoApplication,
  modules,
  src,
}:
buildGoApplication {
  pname = "zoekt-tngl-indexserver";
  version = "0.1.0";
  inherit src modules;

  doCheck = false;

  subPackages = ["cmd/zoekt-tngl-indexserver"];

  meta = {
    mainProgram = "zoekt-tngl-indexserver";
  };
}
