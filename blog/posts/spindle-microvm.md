---
atroot: true
template:
slug: spindle-microvm
title: How the microVM engine comes together
subtitle: spindle has a microVM engine now!
date: 2026-06-16
image: https://assets.tangled.network/blog/seed.png
authors:
  - name: dawn
    email: dawn@tangled.org
    handle: ptr.pet
---

Since launching, [spindle](/ci) has run your CI inside Docker containers created
with nixery. That's been mostly okay if you are doing simple things, but if you
wanted to do anything more outside the box (maybe you wanted some services, or
to build & test containers inside), or if you wanted to use Nix inside it (which
is rough :P), it wouldn't meet your needs. That changes today!

spindle gains a microVM engine. Each workflow gets its own little virtual
machine. You get a full environment inside your workflows that you can do
whatever you want with without any of the roughness of nixery containers.
Alongside this, you also get the ability to configure *services* that a workflow
will have (on the NixOS image), so that means you can easily have postgres,
Docker, and so on that will be alive through the workflow.

## what's in a microVM

A microVM is just a VM with most of the boring parts removed. There's no BIOS,
no PCI bus to probe, no emulated graphics card, none of the slow legacy stuff a
normal QEMU machine drags along for example. You get virtio devices and not much
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

## two kinds of images

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

### example nixos workflow

If you've used spindle before this will look familiar, it's the same manifest you
already know, just with a few extra keys that the NixOS image understands. Here's
a workflow that needs postgres to test against and Docker to build an image:

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

`dependencies` are packages that are added to `environment.systemPackages` (so,
`PATH`). A bare name like `go` is looked up in nixpkgs (same as regular
spindle), but you can also point at any flake with the `flakeref#attr` syntax,
so `github:nixos/nixpkgs#hello` pulls `hello` straight out of that flake.
`registry` is how you remap the global refs: here we pin `nixpkgs` to
`nixos-unstable`, so now the bare `go` above resolves from unstable. You can
alias your own flakes the same way (`myflake: github:me/x`, then `myflake#tool`
in `dependencies`). `caches` is a map of binary cache URL to its trusted public
key, and they get wired into the read proxy (more on that later), so the guest
can substitute prebuilt paths from them instead of building everything from
scratch.

`services` and `virtualisation` are the interesting parts: they're passed
straight through to NixOS, so anything you could write in a NixOS config you can
write here. `services.postgresql.enable` brings postgres up before any of your
steps run. Since steps run as the `spindle-workflow` user, naming a database
after that user with `ensureDBOwnership` is the easy path to a working db -
postgres peer auth maps the unix user straight to the matching role, so `psql`
connects over the socket with no password and no extra setup (this name-matching
is a NixOS requirement for `ensureDBOwnership`, if you want a differently named
db you'd grant access yourself). `virtualisation.docker: true` is shorthand for
`virtualisation.docker.enable = true`, which gets you a real Docker daemon
inside the VM. By the time your first step runs, postgres is listening and the
Docker socket is there, no sidecar dance, it's just part of the machine.

(`true` works as shorthand for `.enable = true` anywhere an `enable` option
exists, so most "just turn this on" services are a one-liner!)

## building the images

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

## finding an image

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

## the life of a workflow

A workflow moves through a handful of stages: it gets parsed and its
image resolved, it waits for a slot, it gets set up, its steps ran, and
then everything is torn down.

The waiting bit matters a lot. Each image declares how much memory, how many
vCPUs and how much disk it needs, and a workflow has to acquire a slot from a
resource scheduler before anything boots. The scheduler is work-conserving with
aging and per-user fairness, so one person submitting a hundred jobs won't
starve everyone else, and slots don't sit idle if there's work that fits in the
budget.

Once a slot is acquired, we do the setup. Spindle allocates a random vsock CID
for the guest and registers it with the agent hub. It creates the per-workflow
work directory, starts the two cache proxies (more on those later), then creates
the VM: writable volumes become sparse files formatted ext4, the store disk is
attached read-only, and QEMU is started with `-sandbox on`, `-nodefaults`, no
display, no monitor, etc. with serial / `virtio_console` output to a log file
and a QMP socket for control.

Then we wait for the machine. We poll QMP until QEMU says the guest is running,
then wait for the agent's handshake to arrive over vsock from the CID we expect.
The agent tells us its protocol and versions, and spindle sends back the job id,
the trusted cache public keys and the cache proxy ports, NixOS config if already
cached... From there steps run one at a time as `$shell -lc <command>`, as the
unprivileged workflow user in `/workspace/repo`, with the right environment and
any unlocked secrets.

Timeouts are cooperative: we work out a deadline from the workflow timeout
and ship it to the guest, with a little grace on our side so the guest
gets a chance to report the timeout itself rather than us just yanking the
machine out from under it. And if the VM crashes mid-step we tail the
serial and QEMU logs into the step's stderr, because "guest agent
connection lost: EOF" is a genuinely useless thing to read at 2am.

Teardown is the same whether the workflow passed, failed or timed out:
drain any pending Nix cache uploads, ask the agent to power off, wait for
QEMU to exit (falling back to a QMP `system_powerdown`, and finally a
kill if it's being stubborn), then close the proxies and remove the work
directory.

## locking down the network

A VM that can reach the host's local network is a VM that can reach things it
has no business reaching. So QEMU doesn't run in the host's network namespace at
all. We `unshare` into fresh user, net and mount namespaces first. Inside that
namespace a small wrapper bind-mounts a resolv.conf pointing at the slirp DNS
and installs blackhole routes for every special-use IP range (RFC 6890, so
private networks, link-local, loopback, etc.) before it execs QEMU.
`slirp4netns` then provides outbound connectivity for that namespace, with
`--disable-host-loopback`, sandbox and seccomp all on. The guest itself sits
behind a *second* layer of QEMU user-mode networking inside that namespace. All
of this is done without needing any privileges!

## budgets and cgroups

The scheduler's budget is bookkeeping on its own, it tracks what it's handed
out, and the runner (QEMU) will ensure that a workflow only gets those. But
optionally the whole thing (QEMU and slirp4netns both) gets placed in a
per-workflow cgroup with memory, swap etc. limits, which is an extra
enforcement layer on top. A nice side effect is when the cgroup OOM-kills the VM
we can see that it was an OOM and report it as such, instead of surfacing it as
a generic crash and leaving you guessing.

The spindle itself also gets a cgroup, which means that in a host OOM situation,
it should be the workflows that die first, not the spindle itself.

## the nix cache, both ways

The two proxies I mentioned during setup are how the guest talks to spindle's
Nix cache, and they run on the host so the guest never needs credentials or
direct network access to do it. Like the agent, they also use vsock to
communicate with the spindle.

The read proxy fans out to the configured substituters plus any caches you
listed in your workflow, so when the guest needs to realize a store path it asks
the proxy and the proxy fetches it. The request is sent concurrently to the read
caches, so the one that answers it first wins.

The upload proxy goes the other way: any path built inside the guest gets pushed
back out to spindle's Nix cache (if one is configured), so the next workflow
that needs it doesn't have to build it again. Any paths that already exist on
any of the configured read caches won't be uploaded. Built paths are queued by
the agent and are immediately uploaded. If any paths are still left when we
reach VM teardown, the workflow will wait until everything is uploaded.

## in the future

todo

Feel free to come and ask any questions you might have on https://chat.tangled.sh!
