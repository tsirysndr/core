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
    rev = "cbaa83aee9dd4aa015fd0c245e1fb3cfbbe32817";
    sha256 = "sha256-QQvkfNjsfU3vReyd8xB2Dtdqninyv5Zem9SuRVTdnK4=";
  };
  subPackages = ["cmd/tap"];
  vendorHash = "sha256-s1S+b+QbptqJ2mxqkvsn7M5VWfLrlwpWgRjg6lq2WVE=";
  doCheck = false;
  meta = {
    mainProgram = "tap";
  };
}
