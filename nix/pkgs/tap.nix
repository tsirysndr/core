{
  buildGoModule,
  fetchFromGitHub,
}:
buildGoModule {
  pname = "tap";
  version = "0.1.0";
  src = fetchFromGitHub {
    owner = "bluesky-social";
    repo = "indigo";
    rev = "498ecb9693e8ae050f73234c86f340f51ad896a9";
    sha256 = "sha256-KASCdwkg/hlKBt7RTW3e3R5J3hqJkphoarFbaMgtN1k=";
  };
  subPackages = ["cmd/tap"];
  vendorHash = "sha256-UOedwNYnM8Jx6B7Y9tFcZX8IeUBESAFAPTRYk7n0yo8=";
  doCheck = false;
  meta = {
    mainProgram = "tap";
  };
}
