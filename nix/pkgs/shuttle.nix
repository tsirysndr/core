{
  rustPlatform,
  src,
  protobuf,
  ...
}: let
  flags = ["--bin" "shuttle" "--package" "shuttle"];
in
  rustPlatform.buildRustPackage {
    pname = "shuttle";
    version = "0.1.0";

    inherit src;

    cargoLock.lockFile = "${src}/Cargo.lock";

    nativeBuildInputs = [
      protobuf
    ];

    cargoBuildFlags = flags;
    cargoTestFlags = flags;
  }
