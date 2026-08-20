package dagger

import "errors"

var (
	ErrOOMKilled = errors.New("oom killed")

	ErrNoRunner = errors.New(
		"the dagger engine needs a runner: set SPINDLE_DAGGER_PIPELINES_RUNNER_HOST " +
			"to an existing dagger engine, or SPINDLE_SERVER_DOCKER_SOCKET so the " +
			"dagger cli can provision one",
	)
)
