{
  rustPlatform,
  src,
  crate,
  cmake,
  perl,
  ...
}:
rustPlatform.buildRustPackage {
  pname = crate;
  version = "2.0.0";

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  nativeBuildInputs = [
    cmake
    perl
  ];

  dontUseCmakeConfigure = true;

  cargoBuildFlags = ["--bin" crate "--package" crate];
  doCheck = false;

  meta.mainProgram = crate;
}
