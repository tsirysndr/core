---
atroot: true
template:
slug: newsletter-03
title: Newsletter 03
subtitle: Code search, focus mode, microVM CI and a new home
image: https://assets.tangled.network/blog/newsletter/03/og.png
date: 2026-07-10
authors:
  - name: Anirudh
    email: anirudh@tangled.org
    handle: anirudh.fi
---

*This was originally sent as an email.*

Happy summer, Tanglers! While the rest of Europe vacations, we've been busy
shipping. Quality of life improvements like Focus Mode, a whole new CI engine
powered by microVMs, code search and a revamped timeline page. Lots to cover,
let's get into it!

## Code search

![Code Search feature
card](https://assets.tangled.network/blog/newsletter/03/CodeSearch.png)

Full-text code search is here. You can now search for code across the
*entire* Tangled network, and yes, regex is supported. :)

Try it here [in our monorepo](https://tangled.org/tangled.org/core/search)!

## Focus mode

![Focus Mode feature
card](https://assets.tangled.network/blog/newsletter/03/Focus.png)

Focus Mode is a new way to clear notifications: lock in and knock out your list
one item at a time, with a task bar that nudges you to stay on track until the
queue is empty.

## Experimental XRPC API

![Experimental API feature
card](https://assets.tangled.network/blog/newsletter/03/API.png)

The Tangled XRPC API is live. An experimental API (using the [AT Protocol XRPC
format](https://atproto.com/specs/xrpc)) to query Tangled is now available at
[api.tangled.org](https://api.tangled.org). Give it a spin:

```
curl https://api.tangled.org/xrpc/sh.tangled.repo.listRepos?subject=did:plc:wshs7t2adsemcrrd4snkeqli | jq
```

You can browse the available lexicons in the monorepo, more docs to follow.

## A new home

![New Home feature
card](https://assets.tangled.network/blog/newsletter/03/Home.png)

 The homepage now uses a brand-new three-column layout: Notifications and
 Recents on the left, your Timeline (Global / Following) in the middle, and
 Trending, Announcements, and more on the right. We hope you like the improved
 information density just as much as we do!

## MicroVM-powered CI

![MicroVM CI feature
card](https://assets.tangled.network/blog/newsletter/03/MicroVM.png)

Spindles can now run jobs inside NixOS/Alpine QEMU microVMs. That means you can
build Docker containers or run services like Postgres directly in your CI
workflows, each job isolated by default.

[Read more →](https://blog.tangled.org/spindle-microvm)

---

That's it for this issue! As always, you can reply to this email, or
find us on [Discord](https://chat.tangled.org).

— Anirudh
