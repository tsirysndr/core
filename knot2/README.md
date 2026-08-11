# Knot 2

This is an alternate implementation of a Tangled knot server!

In Tangled, a "knot" is simply a git server that does its auth layer over the AT Protocol.
Essentially, it tastes like this:

1. Atproto users declare their public SSH key for themselves, in their PDS.
2. A knot admin (defined by atproto DID in the knot config) makes a request to the knot to allow membership to a given atproto user.
3. Said user can now create/update/delete a git repo on the knot, and do normal git things to that git repo over SSH.
4. Said user makes a request to the knot to allow more atproto users as collaborators on their specific git repo.

The reader will notice this disconnect between making requests to the knot, vs. making requests to atproto (individual users' PDSes). The social side of git repos (PRs, issues, etc.) are "owned" by atproto at time of writing; in contrast, important ACL data (members, collaborators), lives on the knot as the source of truth. As time goes on we are re-assessing the idea of users owning what is "collaborative data" (issues, PRs, etc.) on their PDSes - soon may come the day that an issue also lives on the knot as a source of truth, with an accompanying pointer record on user PDS to attest that it's theirs.

Back to knot 2 today:

- **It has no database, or should I say, git is the only database, along with a secret-key-file.**
- It aims to be small, fast, and modern - for example: http3/quic support, SHA256 by default, and no need for unix `git` user, to name a couple of features.
- Config variables are in example.toml, just make a config.toml from that and go ham.

Have fun!

Already running the Go knot and want this instead?
Stop it once and point `knot-migrate` at its database.
Afterwards the knot will serve the same hostname with the same repos,
owners, members and collaborators.
Upgrading the Go knot won't do a magic upgrade for you.
The walkthrough is in [the docs](https://tangled.org/did:plc:j5hmlfdrwkvtxm7cjmu7j2is/blob/master/docs/DOCS.md#migrating-to-knot-2).

# Running a knot2

The following is how I actually run `knot.oyster.cafe`. Please treat it as one possible setup.

The soon-to-be knot operator will need 3 things before beginning:
1. A remote computer, preferably one that stays online
1. A domain name directed at the remote computer
1. The operator's own atproto DID, which becomes the knot's admin

If the operator doesn't know their DID, resolve the handle at [pds.ls](https://pds.ls) or with any atproto tool that does identity resolution.

## Configuration

`example.toml` at project root is generated from the code,
so it always ought to be up to date with what's possible on knot2.
Copy it and fill in the required values:

```sh
cp example.toml config.toml
```

The most important values have no default and thus the knot won't start without them:

- `server.hostname`: Public hostname, which will also be the knot's identity as `did:web:<hostname>`, so it oughtta be a public, good, solid, representative name.
- `server.admins`: List of atproto DIDs, where the first one is the knot's "full owner", which is the DID the operator registers the knot under & what `sh.tangled.owner` returns. Every other admin has the same powers, just no claim to the fancy title, the way tangled is *currently* set up. Admin & minions.
- `server.ssh_host_key_file`: Path for the SSH host key.
- `repo.scan_path`: Path to the directory that gets the actual git repos.
- `secrets.sealed_key_file`: Path to the sealed key store.
- `secrets.master_key_env`: Name of the env var holding the master key.
- `atproto.plc_directory`: PLC directory URL, which is "normally" `https://plc.directory`. There is deliberately no default here because I don't want Bluesky-defaultism. The operator chooses their own if they please.

Some of those can denote files that don't exist yet, because it's more like *where* to put them, since a knot can for sure create files but won't choose paths for the operator. The host key and the sealed store both get created on demand, parent directories and all. Directories are a little different in that `repo.scan_path` and `lfs.store_path` (if the operator sets that) have to already exist and be writable, or the knot won't start. After that, `scan_path` gets filled out as repos arrive.

Every key outside the `[messages]` block can also come from an environment variable, as one can see in the `example.toml`. Environment variables win over the file if both are specified btw.

## Master key

The master key is for unsealing the per-repo signing keys. One has to generate 32 bytes of base64 and keep it out of the config file, for opsec:

```sh
openssl rand -base64 32
```

Put it in an env file that only root can read, then `secrets.master_key_env` at the variable name:

```sh
# knot.env
KNOT_MASTER_KEY=<base64 that was just generated>
```

**Back this up somewhere separate from the server.** The sealed key store on disk is useless without it, and every repo-DID on a knot is derived from keys inside that store. If the operator loses the master key, the repos will of course stay readable as regular git, but the knot will no longer be able to prove ownership to atproto.

## Ports, Taylor's version (ha ha, 22 joke)

A knot has two listeners where HTTP defaults to `[::]:5555`, SSH defaults to `[::]:2222`.

Thus far in Tangled in general, SSH is heavily used because the existing Tangled knot doesn't do HTTP pushing. Tangled generally auto-generates a URL for SSH git'ing that looks like this:

```
git clone git@knot.oyster.cafe:did:plc:barnacle
```

However notice that from Tangled's client, it builds a clone URL with no port number in, because it always assumes 22. If the knot's SSH listener is listening on another port, all of its users have gotta either rewrite the URL as `ssh://git@knot.oyster.cafe:2222/did:plc:barnacle`, or add a block to their `~/.ssh/config`.

So **give the knot port 22 if one can**! It doesn't need unix `git` user / shell account, so the only thing in the way is the remote computer's own sshd, which is usually already taking port 22.

If the reader is nodding along instead of saying "no Lewis I won't change the remote computer's sshd because I like not bricking it" then:

Moving sshd is something that can lock one out of one's own server, so do it in this order:

1. Open a second SSH session to the server and keep it open for this whole procedure. If step 5 goes wrong, having this session open might be the saving grace.
2. Find out which thing owns the port with `systemctl is-enabled ssh.socket`, since each answer sends you to a different file in step 3:
    - `enabled`: edit the socket unit. A `Port` line in `sshd_config` does nothing at all here, and editing it is the usual way to lose an afternoon. Ask me how I know.
    - `disabled`, or no systemd: edit `Port` in `sshd_config`; sshd holds the port itself.
3. Edit `/etc/ssh/sshd_config` and set `Port 2200` or whatever free port one likes. Leave `Port 22` in place as well for now, so sshd listens on both. Under the socket unit, `systemctl edit ssh.socket` takes a `[Socket]` section with `ListenStream=2200` instead, such that the new port joins whatever's already listening.
4. Open the new port in the firewall if there is one. On ufw that's `ufw allow 2200/tcp` (I think!! Untested). On a cloud provider one will probably also have to deal with it / open it in their proprietary config.
5. Restart sshd, or `systemctl restart ssh.socket` for the socket unit, and **from the local computer, in a third terminal**, confirm `ssh -p 2200 root@the.server` works before continuing.
6. Once confirmed, only now remove `Port 22` from `sshd_config`, restart sshd once more, & give the port to the knot. Under the socket unit that's an empty `ListenStream=` above the `ListenStream=2200`, since a drop-in only adds to the port it inherits. When running the binary directly, that means `ssh_listen_addr = "[::]:22"`. Comparatively, in a container it entails publishing the container's 2222 as the host's 22, which is what the compose file example below does.

If one would rather not move sshd at all, another cool option for having knot2 on port 22 is a second IP address on the remote computer. Bind sshd to one with `ListenAddress`, bind the knot to the other with `ssh_listen_addr = "<second-ip>:22"`, and just put the knot's DNS record on that second address. Leaving the knot on `[::]:22` would wildcard-bind every address on the box and would collide with sshd no matter which single IP that sshd listens on.

HTTP can stay on 5555 behind a reverse proxy, or move to 443 if one wants the knot to terminate TLS itself.

## Running under systemd

`systemd/knot.service` at the repository root is the unit I'd install to `/etc/systemd/system/` for running the binary straight on the machine, then `systemctl enable --now knot`.

It runs the knot as `git` from `/usr/local/bin/knot-server`, so edit `User=`, `Group=` and `ExecStart=` if one's setup differs. `ProtectSystem=strict` and `ReadWritePaths=/var/lib/knot` mean anything the knot writes outside the tree gets its own `ReadWritePaths=` entry, an LFS store or an ACME cache on another disk being the usual suspects. `ProtectHome=true` has to go if any of the paths is under `/home`. The two `CAP_NET_BIND_SERVICE` lines let it bind a port below 1024, and both can go when SSH stays on 2222 and HTTP on 5555.

## Running with containers

`knot2/Containerfile` will build a distroless image with `knot-server` and `knot-migrate` in `/usr/local/bin`.
Its `COPY` lines start at the workspace root,
so build it from the repository root and point `-f` at it:

```sh
podman build -f knot2/Containerfile -t knot-oyster:latest .
```

I personally run it with a composefile. This is the file from `knot.oyster.cafe` with a few opsec adjustments:

```yaml
services:
  knot:
    image: localhost/knot-oyster:latest
    container_name: knot-oyster
    pull_policy: never
    restart: unless-stopped
    mem_limit: 2g
    env_file: ./knot.env
    ports:
      - "0.0.0.0:22:2222"
      - "[::]:22:2222"
    volumes:
      - ./config.toml:/etc/knot/config.toml:ro
      - ./repos:/data/repos
      - ./ssh:/data/ssh
      - ./secrets:/data/secrets
      - ./lfs:/data/lfs
```

The container keeps listening on 2222 internally and the host publishes that as 22, so the config file never has to change. My own instance publishes 2222 on the host because that computer already had sshd on 22 when I set it up, and my laziness has been regrettable until knot2 came with http pushing.

Here's the corresponding config, with path specifying the mounted volumes:

```toml
[server]
hostname = "knot.oyster.cafe"
admins = ["did:plc:nel"]  # well obviously this isn't a real DID but one gets the picture
listen_addr = "[::]:5555"
ssh_listen_addr = "[::]:2222"
ssh_host_key_file = "/data/ssh/host_key"
appview_endpoint = "https://tangled.org"  # this is *not* Tangled defaultism, it's cosmetic for git operation messaging

[repo]
scan_path = "/data/repos"

[secrets]
sealed_key_file = "/data/secrets/sealed.bin"
master_key_env = "KNOT_MASTER_KEY"

[atproto]
plc_directory = "https://plc.directory"

[xrpc]
trusted_proxy_header = "x-forwarded-for"

[git]
object_format = "sha256"

[lfs]
store_path = "/data/lfs"
free_space_floor_bytes = 32212254720
```

Create the dirs, then let there be light I suppose:

```sh
mkdir -p repos ssh secrets lfs
podman-compose up -d
```

The `mkdir` is necessary, since the knot won't start unless the repo and LFS directories are present/writable. Podman would create the bind-mount sources for the operator, but then they belong to whichever unix user podman has.

The knot creates the SSH host key on the first run at mode 600, aaand the sealed store on that same first run, because the knot's own signing key needs sealing before any repo exists. The knot purposefully won't load a host key that is group or other readable, so don't loosen those please.

Note that this (my) config turns LFS on, since `lfs.store_path` is set. One can drop that whole `[lfs]` block if one doesn't want it. The floor of 30GiB is what I have judged for my disk (of 500GiB, doing other things at the same time), so pick something that suits one's own instead of copying mine.

Speaking of LFS, I made the directory different in the first place so that we could specify a whole separate storage medium if wanted. For example, let's say I want my actual git repos to be wicked fast, so everything *else* is on an SSD, and *only* LFS is on a massive-but-relatively-cheap HDD cluster. Wouldn't want terabytes and terabytes of massive files taking up precious SSD space in this economy!

## NixOS

`nixosModules.knot-rs` renders the config file, defines the same hardened unit as above, and creates the state directory. A `config.toml` the operator writes goes unread under the module. Add the flake as an input, import the module, and write a `knot.nix`:

```nix
{
  inputs.tangled.url = "git+https://tangled.org/did:plc:j5hmlfdrwkvtxm7cjmu7j2is";
  outputs = {nixpkgs, tangled, ...}: {
    nixosConfigurations.knot = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [tangled.nixosModules.knot-rs ./knot.nix];
    };
  };
}
```

```nix
{
  services.tangled.knot-rs = {
    enable = true;
    environmentFile = "/etc/knot/knot.env";
    settings = {
      server = {
        hostname = "knot.oyster.cafe";
        admins = ["did:plc:nel"];
        ssh_listen_addr = "[::]:22";
      };
      atproto.plc_directory = "https://plc.directory";
      xrpc = {
        trusted_proxy_header = "x-forwarded-for";
        trusted_proxies = ["127.0.0.1" "::1"];
      };
    };
  };

  services.openssh.ports = [2200];
}
```

- `environmentFile` is where `KNOT_MASTER_KEY=` goes, and the module refuses to build without it. Write the path, never the value, so the key stays out of the nix store. Every `KNOT_*` variable the file sets overrides the matching key in `settings`.
- Moving `services.openssh` to another port frees 22 for the knot, per the port dance above, and the module asserts the collision instead of starting two services on one port. It adds `CAP_NET_BIND_SERVICE` itself once a listen address is below 1024. Confirm a session on the new port before rebuilding, since a rebuild that moves sshd and takes 22 in one go will lock one out if the port is wrong.
- `openFirewall` defaults to true and opens the port of each listen address that isn't loopback, so the knot's 22 opens while the default `listen_addr = "127.0.0.1:5555"` stays shut for a proxy on the same host. `services.openssh` opens its own port.
- `stateDir` defaults to `/var/lib/knot`, and `systemd.tmpfiles` creates it and its `repos` at mode 0750 for the `knot` user. `settings.secrets.sealed_key_file` and `settings.server.ssh_host_key_file` default to `sealed-keys` and `ssh_host_key` inside it, and the module works out `ReadWritePaths=` from wherever the operator puts them.
- The module installs the knot and nothing else. [Migrating from the Go knot](https://tangled.org/did:plc:j5hmlfdrwkvtxm7cjmu7j2is/blob/master/docs/DOCS.md#migrating-to-knot-2) runs `knot-migrate` out of `nix build`, before the module is on the machine at all.

## TLS

My setup has Caddy in front, which gets me certificates for free and lets one machine serve several sites (which it does). The config is:

```
knot.oyster.cafe {
	reverse_proxy knot-oyster:5555
}
```

That talks to the knot by container name, which needs both containers on a single podman network. One creates such a network once with something like `podman network create tangled`, then add the network to the compose file above as an external one and to whatever runs the proxy. If one would rather not, publish `127.0.0.1:5555:5555` from the knot container and proxy to that instead.

Set `xrpc.trusted_proxy_header = "x-forwarded-for"` when doing this, otherwise every client looks like it comes from the proxy and the ratelimiter wil treat them as one very busy mister. Only set it behind a proxy the operator controls, since a direct client can like, invent that header.

Add `xrpc.trusted_proxies = ["fd00:1::4", "10.89.0.4"]` for example, one entry per address that the proxy connects from, so the knot honors that header from the proxy alone & ratelimits anyone else by the address they connected from. A CIDR block will work too, `["173.245.48.0/20"]` covers a whole provider's edge. If several proxies you control are in the path, list them all. Mister knot will read the chain right -> left, iterate over every entry the list covers, and take the first entry it doesn't. It will read 32 entries at most, and the knot will ratelimit by the address the request connected from when the list covers all 32.

The knot can also terminate TLS itself (and that's the only way to get its HTTP3 support) because a plain TCP frontend can't proxy QUIC. Using a certificate the operator already manages:

```toml
[server]
listen_addr = "[::]:443"

[tls]
cert_path = "/data/tls/fullchain.pem"
key_path = "/data/tls/privkey.pem"
```

Or let it fetch its own via ACME:

```toml
[server]
listen_addr = "[::]:443"

[tls]
acme_enabled = true
acme_cache_dir = "/data/acme"
acme_contact = "nel@oyster.cafe"
```

ACME here uses the TLS-ALPN-01 challenge, so the knot has to be the reciever (in the phone sense) answering on 443 for the configured hostname. Set `acme_staging = true` while testing such that a typo doesn't nuke the Let's Encrypt ratelimit. Leave `trusted_proxy_header` unset in this mode, and open 443/udp in the firewall if one wants HTTP3 to be reachable.

## First run

```sh
curl -s https://knot.oyster.cafe/xrpc/_health
curl -s https://knot.oyster.cafe/xrpc/sh.tangled.owner
curl -s https://knot.oyster.cafe/.well-known/did.json
```

`_health` reports the version, and if LFS is on it returns an error status when the LFS store isn't writable. `sh.tangled.owner` returns the first DID in `server.admins`, which is what the Tangled appview reads to confirm the operator is who they say they are when they register the knot. `did.json` is the knot's own `did:web` document, served at the hostname it was configured with.

If any of the above endpoints doesn't pong, check out the stderr logs, `podman logs -f knot-oyster` in my case.

## Letting people on

Sharing is caring!

If one wants the knot to show up nicely on Tangled the web app, register the knot on [tangled.org](https://tangled.org) with the same DID listed first in `server.admins`.

Admission is closed by default, so an admin adds each member before they can create repos. Repo owners can then add their own collaborators without any admin involvement of course. If one wants a knot anyone can use:

```toml
[acl]
admission = "open"
```

Note that open admission still respects the blocklist, naturally.

Since ssh-pushing is so popular, users will usually authenticate with the SSH key they published on their PDS -> the most common support question the operator may get is that a user's ssh client agent offers five keys and gets rejected before it ever reaches the registered one. The fix on their side is:

```
Host knot.oyster.cafe
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

## Clone URLs

A git repo can be pushed/pulled by its owner + name, or by its own repo-DID. The owner can be a DID or an atproto handle.

Over SSH:

```sh
git clone knot.oyster.cafe:nel.pet/squid
git clone knot.oyster.cafe:did:plc:nel/squid
git clone knot.oyster.cafe:did:plc:barnacle
```

Over HTTPS, the same:

```sh
git clone https://knot.oyster.cafe/nel.pet/squid
git clone https://knot.oyster.cafe/did:plc:nel/squid
git clone https://knot.oyster.cafe/did:plc:barnacle
```

Push works over both, of course . For HTTP pushing, it is up to the user to find a good Tangled-CLI or something that can put the right things in the git credential helper such that a service auth token is minted and used on push.

A trailing `.git` on the repo name is optional, so `did:plc:nel/squid.git` goes to the same repo as `did:plc:nel/squid`. That only applies to the repo name variant though - `did:plc:barnacle.git` is read as a DID with a `.git` on the end of it, and it won't resolve. This would be made better from better DID parsing, since a `did:plc` can't have dots, only a `did:web` can.

## Things worth knowing before one commits (get it?) to a config

### SHA-256 is the default

Yeah, sorry, let's modernize.

`git.object_format` defaults to `sha256`, and it applies to repos at creation time. A SHA-256 repo cannot be pushed to or fetched from a SHA-1 repo, so if one expects users mirroring in from elsewhere, set `object_format = "sha1"` before anyone creates anything. Changing it later only affects new repos.

### LFS is off until one supplies a path

LFS turns on for both transports when `lfs.store_path` is set. `free_space_floor_bytes` is the disk headroom below which the knot starts refusing uploads, defaulting to 1GiB, so set it to the amount of free space one actually wants to keep.

### Resources tune themselves

`resources.max_threads` and `resources.max_memory_bytes` are smart-ceilings, both `0` by default meaning "use the whole computer". Under a container memory limit the knot reads the cgroup and sizes itself to that, so a `mem_limit` on the container usually suffices.

### Maintenance runs on its own

Commit-graphs, multi-pack indexes, bitmaps, and geometric repacks happen every 6 hours by default. Turn it all off with `maintenance.enabled = false` if one would rather do it oneself.

### The homepage is replaceable, please do replace it

`homepage.path` serves an HTML file of one's choice at `/`, and `homepage.enabled = false` disables the homepage entirely.

## Backups

Back these things up or don't come cryin' to me!

1. The master key, wherever one keeps it.
2. `secrets/sealed.bin`, the sealed key store. It's useless without the master key & the master key is useless without it, so treat them as a pair.
3. `repos/`, which is every repo plus the knot's own ACL data. Git is the database, so that one directory is the knot's entire state.
4. `lfs/`, if one enables it.
5. I guess the SSH host key, though much less dire if it is lost and has to be changed.

## Updating

Pre-built images are at `atcr.io/tangled.org/knot:2`,
with `knot-server` and `knot-migrate` in them.
The image comes from [@tangled.org/knot-docker](https://tangled.org/did:plc:f5s5la5wlofsxidb3zemdune) instead of the `Containerfile` here.
It's a Debian build, with its binaries in `/usr/bin`,
and the `:latest` tag over there is still the Go knot,
so one has to ask for `:2` on purpose.
My composefile above points at a locally built image under `pull_policy: never`.
Switching to the published image means editing the `image:` line,
or tagging the pulled image with the name the compose file already has:

```sh
podman pull atcr.io/tangled.org/knot:2
podman tag atcr.io/tangled.org/knot:2 localhost/knot-oyster:latest
podman-compose up -d
```

The published image bakes `KNOT_SCAN_PATH`,
`KNOT_SEALED_KEY_FILE`,
`KNOT_SSH_HOST_KEY_FILE` and `KNOT_MASTER_KEY_ENV` into itself,
and an environment variable overrides the config file.
The knot will then look for the sealed store at `/data/sealed-keys` and the host key at `/data/ssh_host_key`,
whatever the mounted `config.toml` sets,
find neither,
and generate a new identity and a new host key on a path that isn't mounted by anything.
Put my two paths back in the compose environment before switching to the published image:

```yaml
    environment:
      KNOT_SEALED_KEY_FILE: /data/secrets/sealed.bin
      KNOT_SSH_HOST_KEY_FILE: /data/ssh/host_key
```

The published image also runs as its own `knot` user at uid 1000,
where the distroless image here runs as root,
so chown the four bind mounts before the first start:

```sh
sudo chown -R 1000:1000 repos ssh secrets lfs
```

Skipping the chown will stop the knot at startup with `repo.scan_path /data/repos isn't writable`,
since root wrote every one of the directories.
`user: "0:0"` in the compose file is the other way out,
at the cost of running the knot as root.

Building it oneself is the same three lines as always:

```sh
git pull
podman build -f knot2/Containerfile -t knot-oyster:latest .
podman-compose up -d
```

The knot drains at `SIGTERM` time, so in-flight clones/pushes get up to 40s to finish before they're cut off.

Happy knotting!

