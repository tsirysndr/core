package engine

import (
	"tangled.org/core/spindle/models"
)

// checked before an executor accepts and holds a remote lease
type WorkflowPlacementValidator interface {
	ValidateWorkflowPlacement(wf *models.Workflow) error
}
