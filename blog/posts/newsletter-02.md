---
atroot: true
template:
slug: newsletter-02
title: Newsletter 02
subtitle: Vouching, CI logs over SSH, and more
image: https://assets.tangled.network/blog/newsletter/02/og.png
date: 2026-06-10
authors:
  - name: Anirudh
    email: anirudh@tangled.org
    handle: anirudh.fi
---

*This was originally sent as an email.*

Hello again, Tanglers! It's been a busy few weeks here at Tangled; so
busy in fact that we missed last month's newsletter. :D We've shipped a
lot, so let's get straight to it.

## Vouching and web-of-trust

![vouching graphic](https://assets.tangled.network/vouch.png)

You can now vouch for (or denounce) anyone you interact with on Tangled.
Vouched users show a green shield beside their profile picture;
denounced users get a red one. These "hats" appear at the points where
it matters most -- in issues, PRs, and comments -- so you can make an
informed call before spending time on a review. Vouching is web-of-trust
scoped: you only see decisions made by you and the people you trust.

As always, vouch records are public AT Protocol records stored on your
PDS.

[Read more](https://blog.tangled.org/vouching) 

## SSH log tailing

![astral
projection](https://assets.tangled.network/blog/pty/astral-projection.webp)

You can now tail CI logs right from your terminal! Push to a branch, and
the remote now hands you an SSH command right in your terminal:

```bash
λ jj git push -c @-
  -- snip --
remote: →  Browse CI logs in your terminal:
remote:    ssh -t -p 3333 tangled.org did:plc:j5hmlfdrwkvtxm7cjmu7j2is 796ecc5b0ce5381ed5b5021e7cc28b4b05e03c92
```

Paste that into a new shell and you're dropped into a full-blown TUI
served over SSH -- no installation required. It's built on
[bubbletea](https://github.com/charmbracelet/bubbletea) and
[wish](https://github.com/charmbracelet/wish), so it themes with your
terminal colors and handles ANSI escape codes correctly because it's
writing to a real PTY. 

[Read more](https://blog.tangled.org/ssh)

## New PR page

![pr page
graphic](https://assets.tangled.network/blog/newsletter/02/pr.png)

The pull request creation page got a significant refresh. Go check it out!

## New timeline and notification pages

![timeline
graphic](https://assets.tangled.network/blog/newsletter/02/timeline.png)

We've rebuilt the timeline and notification pages from the ground up.
The timeline page now splits Global and Following feeds, and includes
new Notifications and Recents columns. 

![notifs
graphic](https://assets.tangled.network/blog/newsletter/02/notifications.png)

The dedicated Notifications page now splits "work" and "social"
notifications into two separate feeds, helping declutter your inbox.

## AT Protocol progress

We've made meaningful strides on our "ephemeral appview" goal -- the
milestone where anyone can run a fully backfilled Tangled appview with
minimal compute.

- **Unified feed lexicon.** We consolidated comment and
reaction records into a pair of new lexicon types:
`sh.tangled.feed.comment` and `sh.tangled.feed.reaction`.

- **PR record ingestion.** Pull request records (`sh.tangled.repo.pull`)
are now fully ingested through the firehose. Your agents can create pull
requests by simply writing records to your (or their!?) PDS!

- **Repository renames.** We teased this in our last newsletter: after
switching to DIDs for repo identity, we needed to wire up the rest of
the plumbing. That work is done. You can now rename a repository from
the settings page. Repository migrations between knots are next on this
list.

## VM-based CI is in internal testing

We're currently testing the new spindle engine backed by QEMU micro VMs,
with special niceties for Nix. :^) We're not quite ready to open this up
yet, but expect it to land for everyone in the coming weeks.

---

That's it for this issue! As always, you can reply to this email, or
find us on [Discord](https://chat.tangled.org).

-- Anirudh
