{
  lib,
  rustPlatform,
  tangled,
  ...
}:

rustPlatform.buildRustPackage {
  pname = "bobbin";
  version = "main";

  src = ./.;

  cargoHash = "sha256-apY29WbquD3Eh2yvyRqPPTjm60JZMkty4Vwi1ERZ2+Q=";

  cargoBuildFlags = [
    "--bin"
    "bobbin"
    "--package"
    "bobbin"
  ];

  preBuild = ''
    export BOBBIN_LEXICONS_DIR=${tangled}/lexicons
  '';

  doCheck = false;

  meta = {
    description = "tangled appview";
    homepage = "https://tangled.org/oyster.cafe/bobbin";
    license = lib.licenses.mit;
    mainProgram = "bobbin";
  };
}

