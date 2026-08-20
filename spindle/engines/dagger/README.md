# spindle Dagger engine

Runs a repository's own [Dagger](https://dagger.io) module. A workflow picks it
with `engine: dagger`:

```yaml
engine: dagger
when:
  - event: ["push"]
    branch: ["master"]
steps:
  - name: Test
    command: test --source=.
  - name: Publish
    command: publish --tag=latest
```

`test --source=.` runs `dagger call -m . test --source=.`. The `dagger call`
prefix is implied, and the set of callable names comes from the module at the
commit being built, so there is no second list to keep in sync.

## How a function becomes a command

The engine does not rewrite step commands. Instead, a setup step asks the
module what it exposes and writes one shim per function into a directory that
leads `PATH`:

```
/tangled/dagger/bin/test  ->  exec dagger --progress plain call -m . test "$@"
```

A step is then ordinary `bash -c`, and a function call is an ordinary command
in it — it pipes, chains, and takes arguments like anything else:

```yaml
steps:
  - name: Report
    command: |
      build --source=. > build.log
      test --source=. | tee test.log
      echo "done on $(git rev-parse --short HEAD)"
```

The tradeoff is name resolution. A function that shares a name with a program
in the image shadows it, and a shell builtin (`test`, for one) shadows the
function. `dagger call -m . test` always works.

## Setup steps

Every workflow runs four system steps before the user's own:

| Step                            | What it does                                  |
| ------------------------------- | --------------------------------------------- |
| Clone repository into workspace | the standard clone, into `/tangled/workspace` |
| Install Dagger                  | puts the requested CLI version on `PATH`      |
| Detect Dagger module            | resolves which directory holds the module     |
| Link Dagger functions           | generates the shims                           |

`Install Dagger` resolves a version from the workflow's `version:`, else
`SPINDLE_DAGGER_PIPELINES_VERSION`, else the latest release, and fetches it
from `dl.dagger.io` into `/tangled/dagger/cli`. A CLI already in the image that
reports the wanted version is used as-is, which is how an operator's own image
(`SPINDLE_DAGGER_PIPELINES_IMAGE`) avoids a download it doesn't need.

`Detect Dagger module` looks for the layouts `dagger init` produces: a
`dagger.json` at the repository root (sources under `.dagger`) resolves to `.`,
since `dagger call -m` wants the directory holding `dagger.json`; a module that
keeps its own `dagger.json` inside `.dagger` resolves to `.dagger`. A workflow
overrides all of it with `module:`.

Both steps are shell, so they can be read and tested as shell — see
`setup_steps_test.go`, which runs them against fixture repositories with a
stubbed CLI.

## Running the engine

The CLI inside the workflow container needs a Dagger engine to talk to, from
one of two places:

- `SPINDLE_SERVER_DOCKER_SOCKET` — the socket is bind-mounted into the
  container and the CLI provisions a sibling engine over it.
- `SPINDLE_DAGGER_PIPELINES_RUNNER_HOST` — an engine the operator already runs,
  e.g. `docker-container://dagger-engine` or `tcp://dagger:8080`.

With neither set, `SetupWorkflow` fails with `ErrNoRunner` rather than letting
every step fail deep inside `dagger call`.

The rest of the container is shaped like the nixery engine's: one container per
workflow, `docker exec` per step, state persisted across steps in
`/tangled/workspace`, and the image built on the fly from Nixery unless
`SPINDLE_DAGGER_PIPELINES_IMAGE` pins one.

See [the docs](https://docs.tangled.org/spindles.html) for the full set of
workflow fields and environment variables.
