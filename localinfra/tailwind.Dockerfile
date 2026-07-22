FROM node:22-alpine

WORKDIR /build

RUN npm install --no-save \
      tailwindcss@3.4.17 \
      @tailwindcss/typography@0.5.16 \
      tailwindcss-animated@1.1.2

ENV BROWSERSLIST_IGNORE_OLD_DATA=true

ENTRYPOINT ["node_modules/.bin/tailwindcss"]
