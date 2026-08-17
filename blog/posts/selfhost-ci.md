---
atroot: true
template:
slug: selfhost-ci
title: Run your own CI
subtitle: It is worth it
image: https://assets.tangled.network/blog/rare.png
date: 2026-08-17
authors:
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
---

If the outage of a service prevents you from building and
testing your software, it's time for you to take things into
your own hands.

Hosting your own CI will be worth it. Agents are always new
to codebases and need to constantly test their work to avoid
regressions. They need CI even more than you do, arguably.

Tangled makes self-hosting CI trivial. You can run one by
simply [setting up a
machine](https://docs.tangled.org/spindles#self-hosting-guide)
with our CI runner module. Alternatively, you can also write
your own runner by satisfying the
[interface](https://tangled.org/tangled.org/core/tree/74ce1a0f0c21749bdf15198a4bc7dc457696bf97/lexicons/ci):
[mitchellh.com/tack](https://tangled.org/mitchellh.com/tack)
can proxy builds to Buildkite, Tekton and sourcehut CI. Our
CI engine is infinitely extensible.

Tangled's approach to self-hosted CI is different from the
likes of GitHub. If the app server at Tangled goes down,
your CI jobs continue to schedule and run. As they should!

Unicorns are supposed to be rare.
