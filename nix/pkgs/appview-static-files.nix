{
  runCommandLocal,
  htmx-src,
  htmx-ws-src,
  lucide-src,
  inter-fonts-src,
  ibm-plex-mono-src,
  actor-typeahead-src,
  mermaid-src,
  hls-src,
  mathjax-src,
  tailwindcss,
  tailwindcss-animated-src,
  dolly,
  src,
  linkFarm,
}: let
  tailwindcss-plugins = linkFarm "tailwindcss-plugins" [
    {
      name = "tailwindcss-animated";
      path = tailwindcss-animated-src;
    }
  ];
in
  runCommandLocal "appview-static-files" {
    # TODO(winter): figure out why this is even required after
    # changing the libraries that the tailwindcss binary loads
    sandboxProfile = ''
      (allow file-read* (subpath "/System/Library/OpenSSL"))
    '';
  } ''
    mkdir -p $out/{fonts,icons,logos} && cd $out
    cp -f ${htmx-src} htmx.min.js
    cp -f ${htmx-ws-src} htmx-ext-ws.min.js
    cp -f ${mermaid-src} mermaid.min.js
    cp -f ${hls-src} hls.min.js
    cp -f ${mathjax-src} mathjax.min.js
    cp -rf ${lucide-src}/*.svg icons/
    cp -rf ${src}/icons/*.svg icons/
    cp -f ${inter-fonts-src}/web/InterVariable*.woff2 fonts/
    cp -f ${inter-fonts-src}/web/InterDisplay*.woff2 fonts/
    cp -f ${inter-fonts-src}/InterVariable*.ttf fonts/
    cp -f ${ibm-plex-mono-src}/fonts/complete/woff2/IBMPlexMono*.woff2 fonts/
    cp -f ${actor-typeahead-src}/actor-typeahead.js .
    cp -rf ${src}/appview/pages/profile-fx .

    ${dolly}/bin/dolly -output logos/dolly.png -size 180
    ${dolly}/bin/dolly -output logos/dolly.ico -size 48
    ${dolly}/bin/dolly -output logos/dolly.svg -color currentColor -favicon
    # tailwindcss -c $src/tailwind.config.js -i $src/input.css -o tw.css won't work
    # for whatever reason (produces broken css), so we are doing this instead
    export NODE_PATH=${tailwindcss-plugins}
    cd ${src} && ${tailwindcss}/bin/tailwindcss -i input.css -o $out/tw.css
  ''
