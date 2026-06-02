package db

const (
	KnotMemberUpdateNSID       = "sh.tangled.knot.memberUpdate"
	RepoCollaboratorUpdateNSID = "sh.tangled.repo.collaboratorUpdate"
)

type AclOp string

const (
	AclOpAdd    AclOp = "add"
	AclOpRemove AclOp = "remove"
)

type KnotMemberUpdate struct {
	Op      AclOp  `json:"op"`
	Subject string `json:"subject"`
}

// NOTE: no "addedBy" so for now we can't deduce who to suggest a vouch to, about having added a collaborator.
type RepoCollaboratorUpdate struct {
	Op      AclOp  `json:"op"`
	Subject string `json:"subject"`
	Repo    string `json:"repo"`
}
