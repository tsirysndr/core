package spindle

import (
	"testing"

	"github.com/stretchr/testify/assert"

	"tangled.org/core/api/tangled"
)

func TestFilterWorkflows(t *testing.T) {
	wf := func(name string) *tangled.Pipeline_Workflow {
		return &tangled.Pipeline_Workflow{Name: name}
	}

	tests := []struct {
		name      string
		workflows []*tangled.Pipeline_Workflow
		only      []string
		want      []string
	}{
		{
			name:      "narrows to named workflows",
			workflows: []*tangled.Pipeline_Workflow{wf("ci"), wf("lint"), wf("deploy")},
			only:      []string{"ci", "deploy"},
			want:      []string{"ci", "deploy"},
		},
		{
			name:      "names not present are dropped",
			workflows: []*tangled.Pipeline_Workflow{wf("ci")},
			only:      []string{"ci", "ghost"},
			want:      []string{"ci"},
		},
		{
			name:      "no overlap yields nothing",
			workflows: []*tangled.Pipeline_Workflow{wf("ci")},
			only:      []string{"lint"},
			want:      nil,
		},
		{
			name:      "nil entries are skipped",
			workflows: []*tangled.Pipeline_Workflow{nil, wf("ci")},
			only:      []string{"ci"},
			want:      []string{"ci"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := filterWorkflows(tt.workflows, tt.only)
			var names []string
			for _, w := range got {
				names = append(names, w.Name)
			}
			assert.Equal(t, tt.want, names)
		})
	}
}
