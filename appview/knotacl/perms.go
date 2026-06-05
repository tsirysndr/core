package knotacl

func ownerPermissions() []string {
	return []string{"repo:settings", "repo:push", "repo:owner", "repo:invite", "repo:delete"}
}

func collaboratorPermissions() []string {
	return []string{"repo:collaborator", "repo:settings", "repo:push"}
}

func serverOwnerRepoPermissions() []string {
	return []string{"repo:delete"}
}
