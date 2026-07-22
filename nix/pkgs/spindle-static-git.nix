{
  curlMinimal,
  gitMinimal,
}:
(gitMinimal.override {
  curl = curlMinimal;
  pythonSupport = false;
  withManual = false;
  nlsSupport = false;
}).overrideAttrs (old: {
  doCheck = false;
  doInstallCheck = false;
  configureFlags = (old.configureFlags or []) ++ ["ac_cv_lib_curl_curl_global_init=yes"];
})
