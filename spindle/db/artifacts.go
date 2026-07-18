package db

type FinishedLog struct {
	LeaseID  string
	Workflow string
	Ref      string
	Hash     string
}

func (d *DB) GetFinishedLog(workflow string) (*FinishedLog, error) {
	var fl FinishedLog
	err := d.QueryRow(
		`select lease_id, workflow, ref, hash
		 from mill_artifacts
		 where workflow = ?
		 order by id desc limit 1`,
		workflow,
	).Scan(&fl.LeaseID, &fl.Workflow, &fl.Ref, &fl.Hash)
	if err != nil {
		return nil, err
	}
	return &fl, nil
}

func (d *DB) SaveArtifactRef(leaseID, workflow, ref, hash string) error {
	_, err := d.Exec(
		`insert into mill_artifacts (lease_id, workflow, ref, hash)
		 values (?, ?, ?, ?)`,
		leaseID, workflow, ref, hash,
	)
	return err
}
