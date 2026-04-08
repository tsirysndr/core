---
atroot: true
template:
slug: intro
title: introducing tangled
subtitle: a git collaboration platform, built on atproto
date: 2025-03-02
authors:
  - name: Anirudh
    email: anirudh@tangled.sh
    handle: anirudh.fi
---


[Tangled](https://tangled.sh) is a new social-enabled Git collaboration
platform, built on top of the [AT Protocol](https://atproto.com). We
envision a place where developers have complete ownership of their code,
open source communities can freely self-govern and most importantly,
coding can be social and fun again.

There are several models for decentralized code collaboration platforms,
ranging from ActivityPub's (Forgejo) federated model, to Radicle's
entirely P2P model. Our approach attempts to be the best of both worlds
by adopting atproto -- a protocol for building decentralized social
applications with a central identity.

![tangled architecture](https://assets.tangled.network/blog/arch.svg)

Our approach to this is the idea of "knots". Knots are lightweight,
headless servers that enable users to host Git repositories with ease.
Knots are designed for either single or multi-tenant use which is
perfect for self-hosting on a Raspberry Pi at home, or larger
"community" servers. By default, Tangled provides managed knots where
you can host your repositories for free.

The [App View][appview] at [tangled.sh](https://tangled.sh) acts as a
consolidated "view" into the whole network, allowing users to access,
clone and contribute to repositories hosted across different knots --
completely seamlessly.

Tangled is still in its infancy, and we're building out several of its
core features as we [dogfood it ourselves][dogfood]. We developed these
three tenets to guide our decisions:

1. Ownership of data
2. Low barrier to entry
3. No compromise on user-experience

Collaborating on code isn't easy, and the tools and workflows we use
should feel natural and stay out of the way. Tangled's architecture
enables common workflows to work as you'd expect, all while remaining
decentralized.

We believe that atproto has greatly simplified one of the hardest parts
of social media: having your friends on it. Today, we're rolling out
invite-only access to Tangled -- join us on IRC at `#tangled` on
[libera.chat](https://libera.chat) and we'll get you set up.

**Update**: Tangled is open to public, simply login at
[tangled.sh/login](https://tangled.sh/login)! Have fun!

[pds]: https://atproto.com/guides/glossary#pds-personal-data-server
[appview]: https://docs.bsky.app/docs/advanced-guides/federation-architecture#app-views
[dogfood]: https://tangled.sh/@tangled.sh/core
