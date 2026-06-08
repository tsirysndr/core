---
atroot: true
template:
slug: 6-months
title: 6 months of Tangled
subtitle: A quick recap, and notes on the future
date: 2025-10-21
image: https://assets.tangled.network/blog/6-months.png
authors:
  - name: Anirudh
    email: anirudh@tangled.org
    handle: anirudh.fi
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
draft: false
---

Hello Tanglers! It's been over 6 months since we first announced
Tangled, so we figured we'd do a quick retrospective of what we built so
far and what's next.

If you're new here, here's a quick overview: Tangled is a git hosting
and collaboration platform built on top of the [AT
Protocol](https://atproto.com). You can read a bit more about our
architecture [here](/intro).

## new logo and mascot: dolly!

Tangled finally has a logo! Designed by Akshay himself, Dolly is in
reference to the first ever *cloned* mammal. For a full set of brand assets and guidelines, see our new [branding page](https://tangled.org/brand).

![logo with text](https://assets.tangled.network/blog/logo_with_text.jpeg)

With that, let's recap the major platform improvements so far!

## pull requests: doubling down on jujutsu

One of the first major features we built was our [pull requests
system](/pulls), which follows a unique round-based submission & review
approach. This was really fun to innovate on -- it remains one of
Tangled's core differentiators, and one we plan to keep improving.

In the same vein, we're the first ever code forge to support [stacking
pull requests](/stacking) using Jujutsu! We're big fans of the tool and
we use it everyday as we hack on
[tangled.org/core](https://tangled.org/@tangled.org/core).

Ultimately, we think PR-based collaboration should evolve beyond the
traditional model, and we're excited to keep experimenting with new
ideas that make code review and contribution easier!

## spindle

CI was our most requested feature, and we spent a *lot* of time debating
how to approach it. We considered integrating with existing platforms,
but none were good fits. So we gave in to NIH and [built spindle
ourselves](/ci)! This allowed us to go in on Nix using Nixery to build
CI images on the fly and cache them.

Spindle is still early but designed to be extensible and is AT-native.
The current Docker/Nixery-based engine is limiting -- we plan to switch
to micro VMs down the line to run full-fledged NixOS (and other base
images). Meanwhile, if you've got ideas for other spindle backends
(Kubernetes?!), we'd love to [hear from you](https://chat.tangled.org).

## XRPC APIs

We introduced a complete migration of the knotserver to an
[XRPC](https://atproto.com/specs/xrpc) API. Alongside this, we also
decoupled the knot from the appview by getting rid of the registration
secret, which was centralizing. Knots (and spindles) simply declare
their owner, and any appview can verify ownership. Once we stabilize the
[lexicon definitions](lexicons) for these XRPC calls, building clients
for knots, or alternate implementations should become much simpler.

[lexicons]: https://tangled.sh/@tangled.sh/core/tree/master/lexicons

## issues rework

Issues got a major rework (and facelift) too! They are now threaded:
top-level comments with replies. This makes Q/A style discussions much
easier to follow!

![issue thread](https://assets.tangled.network/blog/issue-threading.webp)

## hosted PDS

A complaint we often received was the need for a Bluesky account to use
Tangled; and besides, we realised that the overlap between Bluesky users
and possible Tangled users only goes so far -- we aim to be a generic
code forge after all, AT just happens to be an implementation
detail.

To address this, we spun up the tngl.sh PDS hosted right here in
Finland. The only way to get an account on this PDS is by [signing
up](https://tangled.sh/signup). There's a lot we can do to improve this
experience as a generic PDS host, but we're still working out details
around that.

## labels

You can easily categorize issues and pulls via labels! There is plenty
of customization available:

- labels can be basic, or they can have a key and value set, for example:
`wontfix` or `priority/high`
- labels can be constrained to a set of values: `priority: [high medium low]`
- there can be multiple labels of a given type: `reviewed-by: @oppi.li`,
`reviewed-by: @anirudh.fi`

The options are endless! You can access them via your repo's settings page.

<div class="flex justify-center items-center gap-2">
  <figure class="w-full m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/labels_vignette.webp">
      <img class="my-1 w-full h-auto cursor-pointer" src="https://assets.tangled.network/blog/labels_vignette.webp" alt="A set of labels applied to an issue.">
    </a>
    <figcaption class="text-center">A set of labels applied to an issue.</figcaption>
  </figure>

  <figure class="w-1/3 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/new_label_modal.png">
      <img class="my-1 w-full h-auto cursor-pointer" src="https://assets.tangled.network/blog/new_label_modal.png" alt="Create custom key-value type labels.">
    </a>
    <figcaption class="text-center">Create custom key-value type labels.</figcaption>
  </figure>
</div>


## notifications

In-app notifications now exist! You get notifications for a variety of events now:

* new issues/pulls on your repos (also for collaborators)
* comments on your issues/pulls (also for collaborators)
* close/reopen (or merge) of issues/pulls
* new stars
* new follows

All of this can be fine-tuned in [/settings/notifications](https://tangled.org/settings/notifications).

![notifications](https://assets.tangled.network/blog/notifications.png)


## the future

We're working on a *lot* of exciting new things and possibly some big
announcements to come. Be on the lookout for:

* email notifications
* preliminary support for issue and PR search
* total "atprotation" [^1] -- the last two holdouts here are repo and pull records
* total federation -- i.e. supporting third-party appviews by making it
  reproducible
* achieve complete independence from Bluesky PBC by hosting our own relay

That's all for now; we'll see you in the atmosphere! Meanwhile, if you'd like to contribute to projects on Tangled, make sure to check out the [good first issues page](https://tangled.org/goodfirstissues) to get started!

[^1]: atprotation implies a two-way sync between the PDS and appview. Currently, pull requests and repositories are not ingested -- so writing/updating either records on your PDS will not show up on the appview.
