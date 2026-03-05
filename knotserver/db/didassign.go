package db

const RepoDIDAssignNSID = "sh.tangled.repo.didAssign"

type RepoDIDAssign struct {
	OwnerDid  string `json:"ownerDid"`
	RepoName  string `json:"repoName"`
	RepoDid   string `json:"repoDid"`
	OldRepoAt string `json:"oldRepoAt,omitempty"`
}
