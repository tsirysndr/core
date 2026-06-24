---
atroot: true
template:
slug: spindle-microvm
title: Spindle's new microVM engine
subtitle: How we built the new QEMU-based microVM engine
date: 2026-06-16
image: https://assets.tangled.network/blog/microvm.png
authors:
  - name: dawn
    email: dawn@tangled.org
    handle: ptr.pet
---

Spindle gains a second engine: `microvm`. Each workflow gets its own little
virtual machine, a whole real environment you can do anything inside. It's an
upgrade from the Nixery engine while staying fully compatible with it, so if you
already have a working Nixery workflow, just change `nixery` to `microvm` and it
will work!

The interesting part is NixOS images: you configure the machine directly from
the workflow file. A few things you can do:

You can bring services up:

```yaml
services:
  postgresql:
    enable: true
    ensureDatabases: ["spindle-workflow"]
    ensureUsers:
      - name: spindle-workflow
        ensureDBOwnership: true
```

You can build Docker containers:

```yaml
virtualisation:
  docker: true
steps:
  - name: "do the thing!"
    command: docker build ...
```

And you can use non-NixOS images too:

```yaml
image: alpine
steps:
  - name: install golang
    command: apk add go
```

It's quick on the second run, too, because it caches aggressively: your
dependencies, your services, and any other Nix derivation built inside the
microVM get pushed to spindle's Nix cache, so the next workflow that needs them
doesn't rebuild those. More on that [below](#the-nix-cache-both-ways).

And like everything else in tangled, the whole thing is self-hostable, so you
can run your own spindle with the microVM engine on your own hardware (see the
[self-hosting
guide](https://docs.tangled.org/spindles.html#self-hosting-guide)). If you want
fuller examples, there are [recipes in the
docs](https://docs.tangled.org/spindles.html#recipes) too.

## What's in a microVM

A microVM is just a VM with most of the boring parts removed. There's no BIOS,
no PCI bus to probe, no emulated graphics card, none of the slow legacy stuff a
normal QEMU machine drags along. You get virtio devices and not much
else, which means it boots very quickly and uses very little memory. Right now
QEMU is the only runner we support, but the engine is written so that other
runners (firecracker for example) can slot in later.

Inside the guest there's a small piece of software we call the agent. Spindle
never SSHes in or runs commands "from the outside"; instead the agent dials back
to spindle over vsock the moment it boots, says hello, and from then on every
step of your workflow is sent to it as a message. The agent runs the command as
an unprivileged user, streams stdout and stderr back, and reports the exit code.
The host side of this lives in
[`spindle`](https://tangled.org/tangled.org/core/tree/master/spindle/engines/microvm/agent.go)
and the guest side is a little Rust binary called
[`shuttle`](https://tangled.org/tangled.org/core/tree/master/shuttle).
(`shuttle` implements
[`agentproto`](https://tangled.org/tangled.org/core/tree/master/spindle/) which
is the protocol used by `spindle`. Technically speaking anyone could implement
this and, assuming side effects hold, you could have your own agent!)

![](https://assets.tangled.network/blog/microvm/diagram1.png)

## Two kinds of images

There are two "flavours" of image you can boot, and they're aimed at fairly
different people.

The first is **NixOS images**. These are the interesting ones: because the whole
guest is built with Nix, you can configure it from your workflow file directly.
Things like `dependencies`, `services`, `virtualisation` (e.g. Docker),
`registry` and `caches` are all written right there in the YAML, and the guest
agent builds and activates that config before any of your steps run. If we've
built that exact base plus config before, spindle can just hand the guest a
store path to realize (fetching from whatever cache `spindle` has configured)
instead of rebuilding it, so the second run is quick.

The second is **non-NixOS images**, which today just means Alpine, but can be
anything. You don't get the workflow-level NixOS config here (there's no NixOS
to configure), but if Nix happens to exist inside the image, like it does in our
Alpine one, it can still talk to the spindle Nix cache just fine.

## An example NixOS workflow

If you've used spindle before, this will look familiar: it's the same manifest
you already know, just with a few extra keys that the NixOS image understands.
Here's a workflow that needs Postgres to test against and Docker to build an
image:

```yaml
# .tangled/workflows/test.yaml
engine: microvm

when:
  - event: ["push", "pull_request"]
    branch: ["master"]

image: nixos

dependencies:
  - go
  - github:nixos/nixpkgs#hello

registry:
  nixpkgs: github:nixos/nixpkgs/nixos-unstable

caches:
  https://nix-community.cachix.org: "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="

services:
  postgresql:
    enable: true
    ensureDatabases: ["spindle-workflow"]
    ensureUsers:
      - name: spindle-workflow
        ensureDBOwnership: true

virtualisation:
  docker: true

steps:
  - name: run tests
    environment:
      PGHOST: /run/postgresql
    command: |
      docker build -t app .
      psql -c "select 1"
      go test ./...
```

The new keys each do one job:

- **`dependencies`** are the packages your steps get to use. They go into a
  `mkShellNoCC` devshell that every step sources before it runs, so you get the
  whole stdenv environment (setup hooks like `pkg-config` wiring up
  `PKG_CONFIG_PATH`, etc.) and not just the bare binaries. That means you can
  use a dependency like `openssl` and compile the `openssl-sys` Rust crate
  without pain! A bare name like `go` is looked up in nixpkgs (same as Nixery),
  but you can also point at any flake with the `flakeref#attr` syntax, so
  `github:nixos/nixpkgs#hello` pulls `hello` straight out of that flake.
- **`registry`** is how you remap the global refs. Here we pin `nixpkgs` to
  `nixos-unstable`, so now the bare `go` above resolves from unstable. You can
  alias your own flakes the same way (`myflake: github:me/x`, then
  `myflake#tool` in `dependencies`).
- **`caches`** is a map of binary cache URL to its trusted public key. They get
  wired into the read proxy (more on that just below), so the guest can
  substitute prebuilt paths from them instead of building everything from
  scratch.

`services` and `virtualisation` are the interesting parts: they're passed
straight through to NixOS, so anything you could write in a NixOS config you can
write here. `services.postgresql.enable` brings Postgres up before any of your
steps run.

Since steps run as the `spindle-workflow` user, naming a database after that
user with `ensureDBOwnership` is the easy path to a working DB -- Postgres peer
auth maps the unix user straight to the matching role, so `psql` connects over
the socket with no password and no extra setup (this name-matching is a NixOS
requirement for `ensureDBOwnership`, if you want a differently named DB you'd
grant access yourself).

`virtualisation.docker: true` is shorthand for `virtualisation.docker.enable =
true`, which gets you a real Docker daemon inside the VM. By the time your first
step runs, Postgres is listening and the Docker socket is there, no sidecar
dance, it's just part of the machine.

(`true` works as shorthand for `.enable = true` anywhere an `enable` option
exists, so most "just turn this on" services are a one-liner!)

## The architecture

![](https://assets.tangled.network/blog/microvm/diagram2.png)

### Nix cache, both ways

Spindle talks to its Nix cache through two proxies that run on the host, so the
guest never needs credentials or direct network access to reach it. Like the
agent, they use vsock to talk to spindle.

The read proxy fans out to the configured substituters plus any caches you
listed in your workflow, so when the guest needs to realize a store path it asks
the proxy and the proxy fetches it. The request is sent concurrently to the read
caches, so the one that answers it first wins.

The upload proxy goes the other way: any path built inside the guest gets pushed
back out to spindle's Nix cache (if one is configured), so the next workflow
that needs it doesn't have to build it again. Any paths that already exist on
any of the configured read caches won't be uploaded. As the agent reports
built paths, they're queued and uploaded in the background while the rest of the
workflow keeps running, so uploads overlap with work instead of blocking it. If
any are still in flight when we reach VM teardown, the workflow waits until
everything has drained.

Spindle can be configured to use `http`, `ssh-ng` or `ssh` URLs as a binary
cache to upload to, so for example, `ssh-ng://localhost` would just upload to
the local Nix store on the machine that the spindle runs on! `ssh-ng` and `ssh`
require Nix to be present in PATH so that the spindle can use `nix copy` to
upload to them, but if you are using a binary cache that supports `http` (for
example, [ncps](https://github.com/kalbasit/ncps)) Nix does not need to be
present.

### Building the images

Image builds are done with Nix. For NixOS we lean on
[microvm.nix](https://github.com/microvm-nix/microvm.nix) and layer our own bits
on top (stripping down kernel modules, configuring users, etc.). For Alpine
there's a smallish Nix definition that fetches the kernel, the initrd and the
kernel modules, sets up an init script that configures the machine on boot,
copies in the dependencies we want (`nix`, `git`, etc.) and compresses the whole
rootfs into a squashfs.

None of this *has* to be Nix, though. As far as spindle is concerned an image is
valid as long as a few things hold: a guest agent (that implements `agentproto`)
is present and gets started on boot, a `spindle-workflow` user exists, and the
work directory is set up at `/workspace`. That can be built however you like.

### Finding an image

Every built image ships a `spec.json` next to its artifacts. The spec is the
whole contract: where the kernel and initrd and read-only store disk live, the
boot args, how much memory and how many vCPUs to give it, the shell to run steps
in, the writable volumes, the network interfaces, and the runner-specific knobs
(machine type, CPU, extra QEMU args). NixOS images also carry a `baseConfigHash`
identifying the base config baked in (this is the hash of
`nixosSystem.config.system.build.toplevel.outPath`).

A workflow picks an image with the `image` key at the top level. The name is
matched literally against what's on disk, we look for a directory called
`<name>` with a `spec.json` in it, then fall back to a flat `<name>.json`. The
nice property here is that resolution depends *only* on the name and what's on
disk, never on the host doing the resolving, so the same workflow resolves to
the same image on every spindle. If an operator keeps multiple arches side by
side they can name them `nixos-x86_64`, `alpine-aarch64` and so on (that suffix
is just part of the name, it's not handled specially). If you want, for example,
`nixos` to work, you can just symlink `nixos` to `nixos-x86_64`.

Right before launch we double-check the referenced files actually exist
and that the host has the tools we need: `mkfs.ext4` for the volumes, the
QEMU binary for the spec's arch, `/dev/kvm` and `/dev/vhost-vsock`, plus
the `ip` / `mount` / `slirp4netns` / `unshare` toolchain if the image
wants networking.

### The life of a workflow

A workflow moves through a handful of stages: it gets parsed and its
image resolved, it waits for a slot, it gets set up, its steps run, and
then everything is torn down.

The waiting bit matters a lot. Each image declares how much memory, how many
vCPUs and how much disk it needs, and a workflow has to acquire a slot from a
resource scheduler before anything boots. The scheduler is work-conserving with
aging and per-user fairness, so one person submitting a hundred jobs won't
starve everyone else, and slots don't sit idle if there's work that fits in the
budget.

Once a slot is acquired, we do the setup. Spindle allocates a random vsock CID
for the guest and registers it with the agent hub. It creates the per-workflow
work directory, starts the two cache proxies (described earlier), a DNS proxy
that resolves through the host and filters out private/special-use addresses,
then creates the VM: writable volumes become sparse files formatted ext4, the
store disk is attached read-only, and QEMU is started with `-sandbox on`,
`-nodefaults`, no display, no monitor, etc. with serial (on boot) /
`virtio_console` output to a log file and a QMP socket for control.

Then we wait for the machine. We poll QMP until QEMU says the guest is running,
then wait for the agent's handshake to arrive over vsock from the CID we expect.
The agent tells us its protocol and versions, and spindle sends back the job id,
the trusted cache public keys, and the cache and DNS proxy ports. From there
steps run one at a time as `$shell -lc <command>`, as the unprivileged workflow
user in `/workspace/repo`, with the right environment and any unlocked secrets.
If the workflow activates a NixOS config and we've already built that exact base
plus config, the activation step can realize a cached toplevel store path instead
of rebuilding. Either way, whether it's building the config fresh or pulling a
cached toplevel down, that output streams straight into the activation step's log
as it happens, so you can watch the closure come in instead of staring at a blank
screen wondering if anything's happening.

Timeouts are cooperative: we work out a deadline from the workflow timeout
and send it to the guest, with a little grace on our side so the guest
gets a chance to report the timeout itself rather than us just yanking the
machine out from under it. And if the VM crashes mid-step we tail the
serial and QEMU logs into the step's stderr, because "guest agent
connection lost: EOF" is a genuinely useless thing to read at 2am...

Teardown is the same whether the workflow passed, failed or timed out:
drain any pending Nix cache uploads, ask the agent to power off, wait for
QEMU to exit (falling back to a QMP `system_powerdown`, and finally a
kill if it's being stubborn), then close the proxies and remove the work
directory.

### Locking down the network

A VM that can reach the host's local network is a VM that can reach things it
has no business reaching. So QEMU doesn't run in the host's network namespace at
all. We `unshare` into fresh user, net and mount namespaces first. Inside that
namespace a small wrapper bind-mounts a resolv.conf pointing at `127.0.0.1` so
that QEMU's built-in slirp DNS isn't used, then installs blackhole routes for
every special-use IP range (RFC 6890, so private networks, link-local, loopback,
etc.) before it execs QEMU. `slirp4netns` then provides the namespace's outbound
internet connection, with `--disable-host-loopback`, sandbox and seccomp all on.
QEMU runs *inside* that namespace, and the guest's network card is attached to
QEMU's own built-in user-mode networking. So every packet from the guest takes
two hops: guest → QEMU's slirp → the namespace's `slirp4netns` → the internet.
The guest never sees the host's network and the host's network never sees the
guest. All of this is done without needing any privileges!

Guest DNS doesn't use either slirp layer. The guest's `/etc/resolv.conf` points
at shuttle on `127.0.0.1:53`, and shuttle forwards DNS packets over vsock to
the host-side DNS proxy. That proxy resolves through the host's real resolver
and strips any answers that point at private or special-use addresses, so guest
traffic can only ever reach the outside world, never the host or anything on its
local networks.

### Budgets and cgroups

The scheduler's budget is bookkeeping on its own, it tracks what it's handed
out, and the runner (QEMU) will ensure that a workflow only gets those. But
optionally the whole thing (QEMU and slirp4netns both) gets placed in a
per-workflow cgroup with memory, swap etc. limits, which is an extra enforcement
layer on top, considering QEMU and slirp4netns themselves also use resources. A
nice side effect is that when the cgroup OOM-kills the VM we can see that it was
an OOM and report it as such, instead of surfacing it as a generic crash and
leaving you guessing.

The spindle itself also gets a cgroup with `memory.min` set, which means that in
a host OOM situation, it should be the workflows that die first, not the spindle
itself.

## On the roadmap

A few things that are coming next:

- [firecracker](https://github.com/firecracker-microvm/firecracker) runner
  support. QEMU microVMs are good and all, but firecracker VMs are more
  efficient to run concurrently and are leaner overall.
- ssh-on-fail: when a workflow fails, you should be able to ssh in to debug why.
  This can be really useful in situations where you need just *a little* bit
  more info if something unexpected fails so you don't sit around there running
  the workflow 10 times over.

Feel free to come and ask any questions you might have on https://chat.tangled.sh!
