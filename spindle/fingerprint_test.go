package spindle

import (
	"testing"

	"github.com/stretchr/testify/assert"

	"tangled.org/core/workflow"
)

func TestFingerprintWorkflowDefinition(t *testing.T) {
	a := workflow.RawWorkflow{Name: "a.yml", Contents: []byte("engine: dummy\n")}
	b := workflow.RawWorkflow{Name: "b.yml", Contents: []byte("engine: nixery\n")}

	// iteration order must not affect the fingerprint
	assert.Equal(t,
		fingerprintWorkflowDefinition(workflow.RawPipeline{a, b}),
		fingerprintWorkflowDefinition(workflow.RawPipeline{b, a}),
	)

	// content and name changes must both change the fingerprint
	base := fingerprintWorkflowDefinition(workflow.RawPipeline{a, b})
	assert.NotEqual(t, base, fingerprintWorkflowDefinition(workflow.RawPipeline{
		{Name: "a.yml", Contents: []byte("engine: nixery\n")}, b,
	}))
	assert.NotEqual(t, base, fingerprintWorkflowDefinition(workflow.RawPipeline{
		{Name: "c.yml", Contents: a.Contents}, b,
	}))

	assert.NotEqual(t,
		fingerprintWorkflowDefinition(nil),
		fingerprintWorkflowDefinition(workflow.RawPipeline{a}),
	)
}
