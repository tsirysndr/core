package db

import (
	"github.com/bluesky-social/indigo/atproto/syntax"
)

type SpindleMember struct {
	Id       int
	Did      syntax.DID // owner of the record
	Rkey     string     // rkey of the record
	Instance string
	Subject  syntax.DID // the member being added
}

func AddSpindleMember(q DBTX, member SpindleMember) error {
	_, err := q.Exec(
		`insert or ignore into spindle_members (did, rkey, instance, subject) values (?, ?, ?, ?)`,
		member.Did,
		member.Rkey,
		member.Instance,
		member.Subject,
	)
	return err
}

func RemoveSpindleMember(q DBTX, ownerDid, rkey string) error {
	_, err := q.Exec(
		"delete from spindle_members where did = ? and rkey = ?",
		ownerDid,
		rkey,
	)
	return err
}

func CountSpindleMembersBySubject(q DBTX, subject string) (int, error) {
	var count int
	err := q.QueryRow(
		`select count(*) from spindle_members where subject = ?`,
		subject,
	).Scan(&count)
	return count, err
}

func GetSpindleMember(q DBTX, did, rkey string) (*SpindleMember, error) {
	query :=
		`select id, did, rkey, instance, subject
		from spindle_members
		where did = ? and rkey = ?`

	var member SpindleMember
	err := q.QueryRow(query, did, rkey).Scan(
		&member.Id,
		&member.Did,
		&member.Rkey,
		&member.Instance,
		&member.Subject,
	)
	if err != nil {
		return nil, err
	}

	return &member, nil
}
