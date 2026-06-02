{
  rustPlatform,
  src,
  ...
}:
let
  flags = ["--bin" "bobbin" "-p" "bobbin"];
in
rustPlatform.buildRustPackage {
  pname = "bobbin";
  version = "0.0.1";

  inherit src;

  cargoLock.lockFile = "${src}/Cargo.lock";

  cargoBuildFlags = flags;
  cargoTestFlags = flags;
}
