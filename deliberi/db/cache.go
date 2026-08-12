package db

func PutRepoName(e Execer, repoDid, ownerDid, name string) error {
	_, err := e.Exec(
		`insert into repo_names (repo_did, name, owner_did) values (?, ?, ?)
		 on conflict(repo_did) do update set name = excluded.name, owner_did = excluded.owner_did`,
		repoDid, name, ownerDid,
	)
	return err
}

func GetRepoName(e Execer, repoDid string) string {
	var name string
	_ = e.QueryRow(`select name from repo_names where repo_did = ?`, repoDid).Scan(&name)
	return name
}

func GetRepoOwner(e Execer, repoDid string) string {
	var owner string
	_ = e.QueryRow(`select owner_did from repo_names where repo_did = ?`, repoDid).Scan(&owner)
	return owner
}

func PutEntityTitle(e Execer, atUri, title string) error {
	_, err := e.Exec(
		`insert into entity_titles (at_uri, title) values (?, ?)
		 on conflict(at_uri) do update set title = excluded.title`,
		atUri, title,
	)
	return err
}

func GetEntityTitle(e Execer, atUri string) string {
	var title string
	_ = e.QueryRow(`select title from entity_titles where at_uri = ?`, atUri).Scan(&title)
	return title
}
