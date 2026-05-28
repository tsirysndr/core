---
atroot: true
template:
slug: ssh
title: tail CI logs over SSH
subtitle: never leave your terminal to check on CI!
image: https://assets.tangled.network/blog/pty/astral-projection.webp
date: 2026-05-28
authors:
  - name: Akshay
    email: akshay@tangled.org
    handle: oppi.li
---

If you push to a new branch on a remote, if CI was
triggered, Tangled now helpfully supplies an `ssh` command
for you:

```bash
λ jj git push -c @-
  -- snip --
remote: →  Browse CI logs in your terminal:
remote:    ssh -t -p 3333 tangled.org did:plc:j5hmlfdrwkvtxm7cjmu7j2is 796ecc5b0ce5381ed5b5021e7cc28b4b05e03c92
```

Paste that command in a new shell, and you are dropped into
a fullblown TUI, served over SSH:

<video src="https://assets.tangled.network/blog/pty/Screen%20Recording%202026-05-27%20at%2011.43.11-cropped-half.webm" controls>
</video>

Try it for yourself:

```
ssh -t -p 3333 tangled.org did:plc:j5hmlfdrwkvtxm7cjmu7j2is 796ecc5b0ce5381ed5b5021e7cc28b4b05e03c92
```

That SSH commmand runs a "screen based" program on the
remote machine. In this case, the program is a TUI built
using
[bubbletea](https://github.com/charmbracelet/bubbletea) and
[wish](https://github.com/charmbracelet/wish).

SSH powered interfaces are wonderful for several reasons:

- They require zero installation
- They work great for interactive bits like menus
- They have all the upsides of running a TUI: you can
  persist them over tmux, theme them with terminal colors
  etc.

But they are doubly wonderful when streaming CI logs, the
screen-program is totally unaware of ANSI escape codes, but
they are handled correctly by virtue of writing to a PTY.
Both ends (the commands running in the CI job and the PTY)
are speaking the same language.
