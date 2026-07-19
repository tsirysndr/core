---
atroot: true
template:
slug: ci
title: Introducing spindle
subtitle: Tangled's new CI runner is now generally available
date: 2025-08-06
authors:
  - name: Anirudh
    email: anirudh@tangled.org
    handle: anirudh.fi
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
---

Since launching Tangled, continuous integration has
consistently topped our feature request list. Today, CI is
no longer a wishlist item, but a fully-featured reality.

Meet **spindle**: Tangled's new CI runner built atop Nix and
AT Protocol. In typical Tangled fashion we've been
dogfooding spindle for a while now; this very blog post
you're reading was [built and published using
spindle](https://tangled.org/tangled.org/core/pipelines).

Tangled is a new social-enabled Git collaboration platform,
[read our intro](/intro) for more about the project.

![spindle architecture](https://assets.tangled.network/blog/spindle-arch.png)

## how spindle works

Spindle is designed around simplicity and the decentralized
nature of the AT Protocol. In ingests "pipeline" records and
emits job status updates.

When you push code or open a pull request, the knot hosting
your repository emits a pipeline event
(`sh.tangled.pipeline`). Running as a dedicated service,
spindle subscribes to these events via websocket connections
to your knot.

Once triggered, spindle reads your pipeline manifest, spins
up the necessary execution environment (covered below), and
runs your defined workflow steps. Throughout execution, it
streams real-time logs and status updates
(`sh.tangled.pipeline.status`) back through websockets,
which the Tangled appview subscribes to for live updates.

Over at the appview, these updates are ingested and stored,
and logs are streamed live.

## spindle pipelines

The pipeline manifest is defined in YAML, and should be
relatively familiar to those that have used other CI
solutions. Here's a minimal example:

```yaml
# test.yaml

when:
  - event: ["push", "pull_request"]
    branch: ["master"]

dependencies:
  nixpkgs:
    - go

steps:
  - name: run all tests
    environment:
      CGO_ENABLED: 1
    command: |
      go test -v ./...
```

You can read the [full manifest spec
here](https://docs.tangled.org/spindles#pipelines),
but the `dependencies` block is the real interesting bit.
Dependencies for your workflow, like Go, Node.js, Python
etc. can be pulled in from nixpkgs.
[Nixpkgs](https://github.com/nixos/nixpkgs/) -- for the
uninitiated -- is a vast collection of packages for the Nix
package manager. Fortunately, you needn't know nor care
about Nix to use it! Just head to https://search.nixos.org
to find your package of choice (I'll bet 1€ that it's
there[^1]), toss it in the list and run your build. The
Nix-savvy of you lot will be happy to know that you can use
custom registries too.

[^1]: I mean, if it isn't there, it's nowhere.

Workflow manifests are intentionally simple. We do not want
to include a "marketplace" of workflows or complex job
orchestration. The bulk of the work should be offloaded to a
build system, and CI should be used simply for finishing
touches. That being said, this is still the first revision
for CI, there is a lot more on the roadmap!

Let's take a look at how spindle executes workflow steps.

## workflow execution

At present, the spindle "engine" supports just the Docker
backend[^2]. Podman is known to work with the Docker socket
feature enabled. Each step is run in a separate container,
with the `/tangled/workspace` and `/nix` volumes persisted
across steps.

[^2]: Support for additional backends like Firecracker are
    planned. Contributions welcome!

The container image is built using
[Nixery](https://nixery.dev). Nixery is a nifty little tool
that takes a path-separated set of Nix packages and returns
an OCI image with each package in a separate layer. Try this
in your terminal if you've got Docker installed:

```
docker run nixery.dev/bash/hello-go hello-go
```

This should output `Hello, world!`. This is running the
[hello-go](https://search.nixos.org/packages?channel=25.05&show=hello-go)
package from nixpkgs.

Nixery is super handy since we can construct these images
for CI environments on the fly, with all dependencies baked
in, and the best part: caching for commonly used packages is
free thanks to Docker (pre-existing layers get reused). We
run a Nixery instance of our own at
https://nixery.tangled.sh but you may override that if you
choose to.

## debugging CI

We understand that debugging CI can be the worst. There are
two parts to this problem: 

- CI services often bring their own workflow definition
  formats and it can sometimes be difficult to know why the
  workflow won't run or why the workflow definition is
  incorrect
- The CI job itself fails, but this has more to do with the
  build system of choice

To mend the first problem: we are making use of git
[push-options](https://git-scm.com/docs/git-push#Documentation/git-push.txt--ooption).
When you push to a repository with an option like so:

```
git push origin master -o verbose-ci
```

The server runs a basic set of analysis rules on your
workflow file, and reports any errors:

```
λ git push origin main -o verbose-ci
  .
  .
  .
  .
remote: error: failed to parse workflow(s):
remote: - at .tangled/workflows/fmt.yml: yaml: line 14: did not find expected key
remote:
remote: warning(s) on pipeline:
remote: - at build.yml: workflow skipped: did not match trigger push
```

The analysis performed at the moment is quite basic (expect
it to get better over time), but it is already quite useful
to help debug workflows that don't trigger!

## pipeline secrets

Secrets are a bit tricky since AT Protocol has no notion of
private data. Secrets are instead written directly from the
appview to the spindle instance using [service
auth](https://docs.bsky.app/docs/api/com-atproto-server-get-service-auth).
In essence, the appview makes a signed request using the
logged-in user's DID key; spindle verifies this signature by
fetching the public key from the DID document.

![pipeline secrets](https://assets.tangled.network/blog/pipeline-secrets.png)

The secrets themselves are stored in a secret manager. By
default, this is the same sqlite database that spindle uses.
This is *fine* for self-hosters. The hosted, flagship
instance at https://spindle.tangled.sh however uses
[OpenBao](https://openbao.org), an OSS fork of HashiCorp
Vault.

## get started now

You can run your own spindle instance pretty easily: the
[spindle self-hosting
guide](https://docs.tangled.org/spindles#self-hosting-guide)
should have you covered. Once done, head to your
repository's settings tab and set it up! Doesn't work? Feel
free to pop into [Discord](https://chat.tangled.org) to get
help -- we have a nice little crew that's always around to
help.

All Tangled users have access to our hosted spindle
instance, free of charge[^3]. You don't have any more
excuses to not migrate to Tangled now -- [get
started](https://tangled.org/login) with your AT Protocol
account today.

[^3]: We can't promise we won't charge for it at some point
    but there will always be a free tier.
