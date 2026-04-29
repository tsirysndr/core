---
atroot: true
template:
slug: federation
title: we need a federation of forges
subtitle: git is decentralized, but what of the rest?
image: https://assets.tangled.network/home-page-self-host-light.svg
date: 2026-04-29
authors:
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
---

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://assets.tangled.network/home-page-self-host-dark.svg">
  <img class="h-auto max-w-full" src="https://assets.tangled.network/home-page-self-host-light.svg">
</picture>

GitHub seems to be crumbling the past couple of weeks.
Whatever the reason, ultimately its not great for 90% of the
world's OSS to depend on one provider. Centralized systems
always crumble; it's the emails, gits, and IRCs that stand
the test of time. Tangled aims to fit in this space, allow
me to explain.

Code collaboration has always made use of two protocols, one
for  code transfer and one for communication:

- It began with the email flow: git (code transfer) + email
  (comms)
- Then there was GitHub: git (code transfer) + GitHub the
  website (comms)
- There is the ForgeFed project: git (code transfer) +
  [maybe
  ActivityPub](https://forgefed.org/blog/actor-programming/)
  (comms)
- We are building Tangled: git (code transfer) + [AT
  protocol](https://atproto.com/) (comms)

Tangled federates events among git servers (called "knots").
You can collaborate on repositories on any server and you
can fork across servers. You can even push to a repository
on your own server, and open a pull-request on a repo hosted
on a completely different server. In a lot of ways, this is
quite like hosting your own cgit instance, and sending out
patches via email.

Tangled uses AT to facilitate the Authenticated Transfer of
events surrounding code: like issues and pull-requests, and
it also enables a few social bits: a timeline of events,
follows, stars (and vouches very soon). AT is used to share
collaborator invites and ssh pubkeys, but the rest is just
good ol' git.

OSS needs to break free from monocultures like GitHub, but
code collaboration should still be fun and social.
