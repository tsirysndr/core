package types

import (
	"testing"

	"tangled.org/core/api/tangled"
)

func TestPipelinesByCommitKeepsNewestFirstPipeline(t *testing.T) {
	newest := &tangled.CiPipeline{Id: "newest", Commit: "shared"}
	older := &tangled.CiPipeline{Id: "older", Commit: "shared"}
	other := &tangled.CiPipeline{Id: "other", Commit: "other"}

	got := PipelinesByCommit([]*tangled.CiPipeline{
		newest,
		nil,
		other,
		older,
	})

	if len(got) != 2 {
		t.Fatalf("got %d mapped commits, want 2: %#v", len(got), got)
	}
	if got["shared"].Id() != "newest" {
		t.Fatalf("shared commit mapped to pipeline %q, want newest", got["shared"].Id())
	}
	if got["other"].Id() != "other" {
		t.Fatalf("other commit mapped to pipeline %q, want other", got["other"].Id())
	}
}
