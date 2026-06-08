---
atroot: true
template:
slug: vouching
title: Combat LLM spam by building a web of trust
subtitle: Vouching on Tangled!
image: https://assets.tangled.network/vouch.png
date: 2026-05-01
authors:
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
draft: false
---

<picture>
  <img class="h-auto max-w-full" src="https://assets.tangled.network/vouch.png">
</picture>

Tangled now has native support for
[vouching](https://github.com/mitchellh/vouch/)! You can
vouch or denounce users that you interact with. Vouched
users will have a green shield icon beside their profile
pictures, and denounced users will have a red one. You can
use this to inform decisions about an interaction. You can
also see the vouch/denounce decisions made by your circle.

## why vouch?

Vouching serves as a signal of trust to your circle.

The bar to submit code to a project has never been lower
thanks to LLM based tooling. LLM tools are really good at
creating "uncanny valley" submissions. Code that looks
correct but is subtly wrong. The onus is on maintainers to
now take the time to review such submissions. To ease this
burden, maintainers from across the Tangled network can now
vouch for or denounce contributors that misuse these tools
and create a maintenance burden.

## mindful design

Such systems need careful consideration. Vouching on Tangled
includes the following to begin with:

- vouching/denouncing with a text-based reason field
- attenuation: you can only view decisions made by you and
  your circle
- no consequences to being denounced: at present, denounced
  users aren't blocked from the project, but simply have a
  red warning label in parts of the UI

Some additions that I want to put in down the line:

- decay of vouches: maintainers and contributors tend to
  move on from projects over time, so vouches should decay
  as time passes, and be renewed every now and then
- evidence trails: vouching for a user right after merging a
  PR should add the PR to the vouch record as a piece of
  evidence

<picture>
  <img class="h-auto max-w-full" src="https://assets.tangled.network/vouching-hats.png">
</picture>

## how it works

When you vouch for or denounce somebody on Tangled, you
create a **public** record on your
[PDS](https://atproto.com/guides/glossary#pds-personal-data-server).
The record includes:

- whether you vouched for or denounced somebody
- an optional reason for doing so

The Tangled appview then aggregates vouch data from across
the network, and displays vouch "hats" over profiles at
points of interaction:

- in issues and issue comments
- in pull-requests and pull-request comments

A hat appears over a user only if you have directly
vouched/denounced them, or if somebody you have vouched for
has vouched/denounced them.

Additionally, there are no consequences for a denounced
user. Only a hat. You can click on the hat to see who
vouched/denounced this user in your circle. The consequences
may change eventually, but for now you can use the hat to
inform a decision.

Start building your web of trust on Tangled today.
