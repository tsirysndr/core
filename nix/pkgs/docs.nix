{
  pandoc,
  pagefind,
  tailwindcss,
  tailwindcss-animated-src,
  linkFarm,
  runCommandLocal,
  inter-fonts-src,
  ibm-plex-mono-src,
  lucide-src,
  dolly,
  src,
}: let
  tailwindcss-plugins = linkFarm "tailwindcss-plugins" [
    {
      name = "tailwindcss-animated";
      path = tailwindcss-animated-src;
    }
  ];
in
  runCommandLocal "docs" {} ''
    mkdir -p working

    # copy templates, themes, styles, filters to working directory
    cp ${src}/docs/*.html working/
    cp ${src}/docs/*.theme working/
    cp ${src}/docs/*.css working/

    # icons
    cp -rf ${lucide-src}/*.svg working/

    # logo
    ${dolly}/bin/dolly -output working/dolly.svg -color currentColor

    # content - chunked
    ${pandoc}/bin/pandoc ${src}/docs/DOCS.md \
      -o $out/ \
      -t chunkedhtml \
      --variable toc \
      --variable-json single-page=false \
      --toc-depth=2 \
      --css=stylesheet.css \
      --chunk-template="%i.html" \
      --highlight-style=working/highlight.theme \
      --template=working/template.html

    # content - single page
    ${pandoc}/bin/pandoc ${src}/docs/DOCS.md \
      -o $out/single-page.html \
      --toc \
      --variable toc \
      --variable single-page \
      --toc-depth=2 \
      --css=stylesheet.css \
      --highlight-style=working/highlight.theme \
      --template=working/template.html

    # fonts
    mkdir -p $out/static/fonts
    cp -f ${inter-fonts-src}/web/InterVariable*.woff2 $out/static/fonts/
    cp -f ${inter-fonts-src}/web/InterDisplay*.woff2 $out/static/fonts/
    cp -f ${inter-fonts-src}/InterVariable*.ttf $out/static/fonts/
    cp -f ${ibm-plex-mono-src}/fonts/complete/woff2/IBMPlexMono*.woff2 $out/static/fonts/

    # favicons
    ${dolly}/bin/dolly -output $out/static/logos/dolly.png -size 180
    ${dolly}/bin/dolly -output $out/static/logos/dolly.ico -size 48
    ${dolly}/bin/dolly -output $out/static/logos/dolly.svg -color currentColor -favicon

    # styles
    export NODE_PATH=${tailwindcss-plugins}
    cd ${src} && ${tailwindcss}/bin/tailwindcss -i input.css -o $out/stylesheet.css

    # search index
    ${pagefind}/bin/pagefind --site $out --output-path $out/pagefind
  ''
