---
atroot: true
template:
slug: stacking
title: jujutsu on tangled
subtitle: tangled now supports jujutsu change-ids!
date: 2025-06-02
image: https://assets.tangled.network/blog/interdiff_difference.jpeg
authors:
  - name: Akshay
    email: akshay@tangled.sh
    handle: oppi.li
draft: false
---

Jujutsu is built around structuring your work into
meaningful commits. Naturally, during code-review, you'd
expect reviewers to be able to comment on individual
commits, and also see the evolution of a commit over time,
as reviews are addressed. We set out to natively support
this model of code-review on Tangled.

Tangled is a new social-enabled Git collaboration platform,
[read our intro](/intro) for more about the project.

For starters, I would like to contrast the two schools of
code-review, the "diff-soup" model and the interdiff model.

## the diff-soup model

When you create a PR on traditional code forges (GitHub
specifically), the UX implicitly encourages you to address
your code review by *adding commits* on top of your original
set of changes:

- GitHub's "Apply Suggestion" button directly commits the
  suggestion into your PR
- GitHub only shows you the diff of all files at once by
  default
- It is difficult to know what changed across force pushes

Consider a hypothetical PR that adds 3 commits:

```
[c] implement new feature across the board  (HEAD)
 |
[b] introduce new feature
 |
[a] some small refactor
```

And when only newly added commits are easy to review, this
is what ends up happening:

```
[f] formatting & linting  (HEAD)
 |
[e] update name of new feature
 |
[d] fix bug in refactor
 |
[c] implement new feature across the board
 |
[b] introduce new feature
 |
[a] some small refactor
```

It is impossible to tell what addresses what at a glance,
there is an implicit relation between each change:

```
[f] formatting & linting
 |
[e] update name of new feature -------------.
 |                                          |
[d] fix bug in refactor -----------.        |
 |                                 |        |
[c] implement new feature across the board  |
 |                                 |        |
[b] introduce new feature <-----------------'
 |                                 |
[a] some small refactor <----------'
```

This has the downside of clobbering the output of `git
blame` (if there is a bug in the new feature, you will first
land on `e`, and upon digging further, you will land on
`b`). This becomes incredibly tricky to navigate if reviews
go on through multiple cycles.


## the interdiff model

With jujutsu however, you have the tools at hand to
fearlessly edit, split, squash and rework old commits (you
can absolutely achieve this with git and interactive
rebasing, but it is certainly not trivial).

Let's try that again:

```
[c] implement new feature across the board  (HEAD)
 |
[b] introduce new feature
 |
[a] some small refactor
```

To fix the bug in the refactor:

```
$ jj edit a
Working copy  (@) now at: [a] some small refactor

$ # hack hack hack

$ jj log -r a::
Rebased 2 descendant commits onto updated working copy
[c] implement new feature across the board  (HEAD)
 |
[b] introduce new feature
 |
[a] some small refactor
```

Jujutsu automatically rebases the descendants without having
to lift a finger. Brilliant! You can repeat the same
exercise for all review comments, and effectively, your
PR will have evolved like so:

```
     a  ->  b  ->  c      initial attempt
     |      |      |
     v      v      v
     a' ->  b' ->  c'     after first cycle of reviews
```

## the catch

If you use `git rebase`, you will know that it modifies
history and therefore changes the commit SHA. How then,
should one tell the difference between the "old" and "new"
state of affairs?

Tools like `git-range-diff` make use of a variety of
text-based heuristics to roughly match `a` to `a'` and `b`
to `b'` etc.

Jujutsu however, works around this by assigning stable
"change id"s to each change (which internally point to a git
commit, if you use the git backing). If you edit a commit,
its SHA changes, but its change-id remains the same.

And this is the essence of our new stacked PRs feature!

## interdiff code review on tangled

To really explain how this works, let's start with a [new
codebase](https://tangled.sh/@oppi.li/stacking-demo/):

```
$ jj git init --colocate

# -- initialize codebase --

$ jj log
@  n set: introduce Set type main HEAD 1h
```

I have kicked things off by creating a new go module that
adds a `HashSet` data structure. My first changeset
introduces some basic set operations:

```
$ jj log
@  so set: introduce set difference HEAD
├  sq set: introduce set intersection
├  mk set: introduce set union
├  my set: introduce basic set operations
~

$ jj git push -c @
Changes to push to origin:
  Add bookmark push-soqmukrvport to fc06362295bd
```

When submitting a pull request, select "Submit as stacked PRs":

<figure class="max-w-[550px] m-auto flex flex-col items-center justify-center">
  <a href="https://assets.tangled.network/blog/submit_stacked.jpeg">
    <img class="my-1 h-auto max-w-full" src="https://assets.tangled.network/blog/submit_stacked.jpeg">
  </a>
  <figcaption class="text-center">Submitting Stacked PRs</figcaption>
</figure>

This submits each change as an individual pull request:

<figure class="max-w-[550px] m-auto flex flex-col items-center justify-center">
  <a href="https://assets.tangled.network/blog/top_of_stack.jpeg">
    <img class="my-1 h-auto max-w-full" src="https://assets.tangled.network/blog/top_of_stack.jpeg">
  </a>
  <figcaption class="text-center">The "stack" is similar to Gerrit's relation chain</figcaption>
</figure>

After a while, I receive a couple of review comments, not on
my entire submission, but rather, on each *individual
change*. Additionally, the reviewer is happy with my first
change, and has gone ahead and merged that:

<div class="flex justify-center items-start gap-2">
  <figure class="w-1/3 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/basic_merged.jpeg">
      <img class="my-1 w-full h-auto cursor-pointer" src="https://assets.tangled.network/blog/basic_merged.jpeg" alt="The first change has been merged">
    </a>
    <figcaption class="text-center">The first change has been merged</figcaption>
  </figure>

  <figure class="w-1/3 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/review_union.jpeg">
      <img class="my-1 w-full h-auto cursor-pointer" src="https://assets.tangled.network/blog/review_union.jpeg" alt="A review on the set union implementation">
    </a>
    <figcaption class="text-center">A review on the set union implementation</figcaption>
  </figure>

  <figure class="w-1/3 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/review_difference.jpeg">
      <img class="my-1 w-full h-auto cursor-pointer" src="https://assets.tangled.network/blog/review_difference.jpeg" alt="A review on the set difference implementation">
    </a>
    <figcaption class="text-center">A review on the set difference implementation</figcaption>
  </figure>
</div>

Let us address the first review:

> can you use the new `maps.Copy` api here?

```
$ jj log
@  so set: introduce set difference push-soqmukrvport
├  sq set: introduce set intersection
├  mk set: introduce set union
├  my set: introduce basic set operations
~

# let's edit the implementation of `Union`
$ jj edit mk

# hack, hack, hack

$ jj log
Rebased 2 descendant commits onto updated working copy
├  so set: introduce set difference push-soqmukrvport*
├  sq set: introduce set intersection
@  mk set: introduce set union
├  my set: introduce basic set operations
~
```

Next, let us address the bug:

> there is a logic bug here, the condition should be negated.

```
# let's edit the implementation of `Difference`
$ jj edit so

# hack, hack, hack
```

We are done addressing reviews:
```
$ jj git push
Changes to push to origin:
  Move sideways bookmark push-soqmukrvport from fc06362295bd to dfe2750f6d40
```

Upon resubmitting the PR for review, Tangled is able to
accurately trace the commit across rewrites, using jujutsu
change-ids, and map it to the corresponding PR:

<div class="flex justify-center items-start gap-2">
  <figure class="w-1/2 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/round_2_union.jpeg">
      <img class="my-1 w-full h-auto" src="https://assets.tangled.network/blog/round_2_union.jpeg" alt="PR #2 advances to the next round">
    </a>
    <figcaption class="text-center">PR #2 advances to the next round</figcaption>
  </figure>

  <figure class="w-1/2 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/round_2_difference.jpeg">
      <img class="my-1 w-full h-auto" src="https://assets.tangled.network/blog/round_2_difference.jpeg" alt="PR #4 advances to the next round">
    </a>
    <figcaption class="text-center">PR #4 advances to the next round</figcaption>
  </figure>
</div>

Of note here are a few things:

- The initial submission is still visible under `round #0`
- By resubmitting, the round has simply advanced to `round
  #1`
- There is a helpful "interdiff" button to look at the
  difference between the two submissions

The individual diffs are still available, but most
importantly, the reviewer can view the *evolution* of a
change by hitting the interdiff button:

<div class="flex justify-center items-start gap-2">
  <figure class="w-1/2 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/diff_1_difference.jpeg">
      <img class="my-1 w-full h-auto" src="https://assets.tangled.network/blog/diff_1_difference.jpeg" alt="Diff from round #0">
    </a>
    <figcaption class="text-center">Diff from round #0</figcaption>
  </figure>

  <figure class="w-1/2 m-0 flex flex-col items-center">
    <a href="https://assets.tangled.network/blog/diff_2_difference.jpeg">
      <img class="my-1 w-full h-auto" src="https://assets.tangled.network/blog/diff_2_difference.jpeg" alt="Diff from round #1">
    </a>
    <figcaption class="text-center">Diff from round #1</figcaption>
  </figure>
</div>

<figure class="max-w-[550px] m-auto flex flex-col items-center justify-center">
  <a href="https://assets.tangled.network/blog/interdiff_difference.jpeg">
    <img class="my-1 w-full h-auto" src="https://assets.tangled.network/blog/interdiff_difference.jpeg" alt="Interdiff between round #0 and #1">
  </a>
  <figcaption class="text-center">Interdiff between round #1 and #0</figcaption>
</figure>

Indeed, the logic bug has been addressed!

## start stacking today

If you are a jujutsu user, you can enable this flag on more
recent versions of jujutsu:

```
λ jj --version
jj 0.29.0-8c7ca30074767257d75e3842581b61e764d022cf

# -- in your config.toml file --
[git]
write-change-id-header = true
```

This feature writes `change-id` headers directly into the
git commit object, and is visible to code forges upon push,
and allows you to stack your PRs on Tangled.
