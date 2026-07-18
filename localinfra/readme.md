Heavily inspired by [frontpage dev environment](https://github.com/frontpagefyi/frontpage/blob/10678df9c3f72cbd82f0856a9f99c74dd22326d8/apps/frontpage/local-infra/README.md).
Tangled's setup is slightly more involved because services inside the network need to reach the PDS over its **public** hostname with **valid TLS** — federation paths (DID resolution, OAuth, etc.) round-trip through the same URLs an external client would use.

For example, resolving `alice.pds.tngl.boltless.dev` yields an `#atproto_pds` service pointing at `https://pds.tngl.boltless.dev`. Knot and spindle running inside docker must hit that exact URL and trust its cert.

To make that work:

- Caddy's dev root CA is mounted into every container that talks to another service over HTTPS.
- The Docker network uses an unrouted "public" subnet so the SSRF dialer doesn't reject container IPs as private.

## What's inside:

- [did-method-plc](https://github.com/did-method-plc/did-method-plc) (<https://plc.tngl.boltless.dev>)
- atproto_pds (<https://pds.tngl.boltless.dev>)
- jetstream (<https://jetstream.tngl.boltless.dev>)
- knot (<https://knot.tngl.boltless.dev>)
- spindle (<https://spindle.tngl.boltless.dev>)
- knotmirror (<https://knotmirror.tngl.boltless.dev>)
- appview (<https://tngl.boltless.dev>) (live reloading)
- [ncps](https://github.com/kalbasit/ncps) nix binary cache (internal, `http://ncps:8501`)
- pdsls (<https://pdsls.tngl.boltless.dev>)
- caddy reverse proxy

## Setup

1. Generate the dev CA from the repo root:
    ```bash
    mkdir -p localinfra/certs &&
    openssl req -x509 -newkey rsa:2048 \
        -keyout localinfra/certs/root.key \
        -out localinfra/certs/root.crt \
        -days 3650 -nodes \
        -subj "/CN=Tangled Dev CA" \
        -addext "basicConstraints=critical,CA:TRUE,pathlen:1" \
        -addext "keyUsage=critical,keyCertSign,cRLSign" \
        -addext "nameConstraints=critical,permitted;DNS:tngl.boltless.dev"
    ```
2. Trust generated `localinfra/certs/root.crt` in your system's trust store.
  - For example in MacOS, run
    ```bash
    sudo security add-trusted-cert -d -r trustRoot \
      -k /Library/Keychains/System.keychain \
      ./localinfra/certs/root.crt
    ```
  - Depending on your browser you may have to import the certificate into your browser profiles too as some have their own certs do not use your system ones
3. run `./localinfra/scripts/appview-static-files.sh`
4. Prepare the spindle microVM images:
    ```bash
    ./localinfra/scripts/prepare-spindle-images.sh
    ```
    This writes the image directory under `out/localinfra-spindle-images`.
5. `docker compose up`
6. AppView will be running on `127.0.0.1:3000` with two test users: `alice.pds.tngl.boltless.dev` and `bob.pds.tngl.boltless.dev`. Both with password `password`.
`TANGLED_APPVIEW_HOST` must be a loopback IP with the mapped port (`127.0.0.1:3000`), not `localhost`: atproto's dev OAuth client requires a loopback IP for the redirect URI. If you remap the published appview port, update `TANGLED_APPVIEW_HOST` in `docker-compose.yml` to match.


## Mill mode

The default stack runs one standalone spindle. `docker-compose.mill.yml` is an overlay that splits that into the distributed arch: the primary spindle becomes the mill host (`role=mill`, it only places jobs) and a three-executor fleet (`role=executor`) runs the engines.

```bash
docker compose -f docker-compose.yml -f docker-compose.mill.yml --profile linux up
```

The executors differ on the two axes placement cares about, so you can watch candidates get filtered and ranked:

| executor   | labels        | images      | runs                        |
|------------|---------------|-------------|-----------------------------|
| executor-a | `linux, fast` | full        | `image/alpine, image/nixos` |
| executor-b | `linux, slow` | full        | `image/alpine, image/nixos` |
| executor-c | `linux, gpu`  | alpine only | `image/alpine`              |

So `image: nixos` has two candidates, `image: alpine` has three, and `runs_on: [gpu]` pins to executor-c. The alpine-only image set is staged by `prepare-spindle-images.sh` (step 4) alongside the full one, no extra step.

Each executor needs its own identity (one live session per token), so the `mill-tokens` service registers a token per executor in the mill db and drops it into the shared volume for the executor to read.
