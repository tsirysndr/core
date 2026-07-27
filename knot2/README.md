# Knot 2

> Lewis 🦪
>
> Not sure if this will be upstreamed by Tangled the company, but I'm mentioning that possibility if it concerns the reader.
> I work at Tangled after all!

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

1. Open a second SSH session to the server and keep it open for this whole procedure. If step 4 goes wrong, having this session open might be the saving grace.
2. Edit `/etc/ssh/sshd_config` and set `Port 2200` or whatever free port one likes. Leave `Port 22` in place as well for now, so sshd listens on both.
3. Open the new port in the firewall if there is one. On ufw that's `ufw allow 2200/tcp` (I think!! Untested). On a cloud provider one will probably also have to deal with it / open it in their proprietary config.
4. Restart sshd, and **from the local computer, in a third terminal**, confirm `ssh -p 2200 root@the.server` works before continuing.
5. Once confirmed, only now remove `Port 22` from `sshd_config`, restart sshd once more, & give the port to the knot. When running the binary directly, that means `ssh_listen_addr = "[::]:22"`. Comparatively, in a container it entails publishing the container's 2222 as the host's 22, which is what the compose file example below does.

If one would rather not move sshd at all, another cool option for having knot2 on port 22 is a second IP address on the remote computer. Bind sshd to one with `ListenAddress`, bind the knot to the other with `ssh_listen_addr = "<second-ip>:22"`, and just put the knot's DNS record on that second address. Leaving the knot on `[::]:22` would wildcard-bind every address on the box and would collide with sshd no matter which single IP that sshd listens on.

HTTP can stay on 5555 behind a reverse proxy, or move to 443 if one wants the knot to terminate TLS itself.

## Running with containers

The `Containerfile` at project root builds a distroless image with just the `knot-server` binary in it:

```sh
podman build -t knot-oyster:latest .
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

The `mkdir` is necessary, since the knot won't start unless the repo and LFS directories are present/writable. Podman would create the bind-mount sources for the operator, but then they belong to whichever unix user podman has rather than to the operator.

The knot creates the SSH host key on the first run at mode 600, aaand the sealed store on that same first run, because the knot's own signing key needs sealing before any repo exists. The knot purposefully won't load a host key that is group or other readable, so don't loosen those please.

Note that this (my) config turns LFS on, since `lfs.store_path` is set. One can drop that whole `[lfs]` block if one doesn't want it. The floor of 30GiB is what I have judged for my disk (of 500GiB, doing other things at the same time), so pick something that suits one's own rather than copying mine.

Speaking of LFS, I made the directory different in the first place so that we could specify a whole separate storage medium if wanted. For example, let's say I want my actual git repos to be wicked fast, so everything *else* is on an SSD, and *only* LFS is on a massive-but-relatively-cheap HDD cluster. Wouldn't want terabytes and terabytes of massive files taking up precious SSD space in this economy!

## TLS

My setup has Caddy in front, which gets me certificates for free and lets one machine serve several sites (which it does). The config is:

```
knot.oyster.cafe {
	reverse_proxy knot-oyster:5555
}
```

That talks to the knot by container name, which needs both containers on a single podman network. One creates such a network once with something like `podman network create tangled`, then add the network to the compose file above as an external one and to whatever runs the proxy. If one would rather not, publish `127.0.0.1:5555:5555` from the knot container and proxy to that instead.

Set `xrpc.trusted_proxy_header = "x-forwarded-for"` when doing this, otherwise every client looks like it comes from the proxy and the ratelimiter wil treat them as one very busy mister. Only set it behind a proxy the operator controls, since a direct client can like, invent that header.

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

A trailing `.git` on the repo name is optional, so `did:plc:nel/squid.git` goes to the same repo as `did:plc:nel/squid`. That only applies to the repo name variant though - `did:plc:barnacle.git` is read as a DID rather than as a repo-DID with a suffix, and it won't resolve. This would be made better from better DID parsing, since a `did:plc` can't have dots, only a `did:web` can.

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

// TODO: publish to ATCR, maybe nix something something.

```sh
git pull
podman build -t knot-oyster:latest .
podman-compose up -d
```

The knot drains at `SIGTERM` time, so in-flight clones/pushes get up to 40s to finish before they're cut off.

Happy knotting!

