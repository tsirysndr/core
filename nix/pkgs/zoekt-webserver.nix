{
  lib,
  buildGoModule,
  fetchFromTangled,
}:
buildGoModule {
  pname = "zoekt-webserver";
  version = "0-unstable-2026-03-25";

  src = fetchFromTangled {
    did = "did:plc:j7br4gn6mwy72wd6skzjhwh5";
    rev = "cfc4aa0d9ea620143fef099bfed61a88e434c9cd";
    hash = "sha256-BizF1KGkNecAmc31KuC0rlwLY9dHaYtyohSwTUzX5CE=";
  };
  subPackages = ["cmd/zoekt-webserver"];

  vendorHash = "sha256-pWzVhu5nY4e97dHuw5ncLP8YkiYjk0m9l70GYizLNj8=";

  doCheck = false;

  meta = {
    description = "Fast trigram based code search";
    homepage = "https://github.com/sourcegraph/zoekt";
    license = lib.licenses.asl20;
    maintainers = [];
    mainProgram = "zoekt-webserver";
  };
}
