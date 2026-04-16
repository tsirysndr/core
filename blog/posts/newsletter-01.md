---
atroot: true
template:
slug: newsletter-01
title: newsletter 01 — hello!
subtitle: kicking off our newsletter!
date: 2026-04-16
authors:
  - name: Anirudh
    email: anirudh@tangled.org
    handle: anirudh.fi
---

*This was originally sent as an email.*

![newsletter
graphic](https://assets.tangled.network/blog/newsletter-01.png)

Hello Tanglers! We're excited to kick off the first ever issue of our
newsletter. We haven't settled on a cutesy name for it yet --
"untangled"? Or simply "tangled weekly"? Let us know!

If you're new here (how'd you end up on our list?!), Tangled is a new
social coding platform. You can host Git repositories, collaborate with
friends, and self-host your git remotes
([knots](https://docs.tangled.org/knot-self-hosting-guide.html)) and CI
runners (spindles). We recently raised €3,8M ($4.5M) in [seed
financing](/seed) and have been heads-down building.

With that out of the way, here's what we shipped recently.

## Repository search is live

![search
graphic](https://assets.tangled.network/blog/search-graphic.png)

We finally have it. You can now search for your favourite slopware at
[tangled.org/search](https://tangled.org/search) (or from the 🔍 icon in
the top nav).

## Fresh new OpenGraph images

![og graphic](https://assets.tangled.network/blog/og-graphic.png)

[Étienne](https://tangled.org/eti.tf) helped us ship `ogre` -- the new
OpenGraph Rendering Engine. It's what's behind our lovely link previews
that you may have seen elsewhere already.

## PR review improvements

![review flow](https://assets.tangled.network/blog/reviewflow.gif)

[Lewis](https://tangled.org/oyster.cafe), our most recent addition to
the team (based out of Helsinki!), shipped these handy PR review
improvements! You can multi-line select to reference relevant code in
review comments, and mark files that you've already looked at as
"reviewed". Nifty!

## knotmirror and other planned features

[Seongmin](https://tangled.org/boltless.me), our engineer from South
Korea 🇰🇷, built our new repository indexer service: knotmirror.

Knotmirror is our first of many performance optimizations for repo
viewing & fetching on tangled. It attempts to fetch and index all known
git repos across knots, allowing the appview (the main service at
tangled.org) to serve repos much faster, regardless of where your knot
lives!

Up next, we're working hard towards:

* First implementation of **VM-based CI** to enable more advanced
  workflows. This will be a new spindle engine likely backed by QEMU. At
  least initially. We'll let you know when we ship it!
* **Vouching**: Tangled aims to be foundational infrastructure for the
  future of open source dev -- and having first-class tooling for the
  slopocalypse is paramount. The web-of-trust model has proven to be
  rather powerful here and that's coming to Tangled very soon.
* Fully achieve full **AT Protocol compliance**. One of our internal
  goals is to have a Tangled appview that anyone can run, with complete
  backfill and ingestion -- dubbed the "ephemeral appview". We're not quite
  there yet: repos are the tough part. We recently swapped to using DIDs
  to identify repos, which gets us one step closer to being able to
  rename them, transfer them between knots, and finally ingest them
  through the firehose.

That's all for now! We'd love to hear your thoughts on this newsletter
-- you can reply to this email or join our
[Discord](https://chat.tangled.org) to let us know. We'll also publish
this on our blog for those that prefer RSS.

-- Anirudh
