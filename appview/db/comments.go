package db

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
)

func PutComment(tx *sql.Tx, c *models.Comment, references []syntax.ATURI) error {
	if c.Collection == "" {
		c.Collection = tangled.FeedCommentNSID
	}

	var bodyBlobs, replyToUri, replyToCid *string
	if len(c.Body.Blobs) > 0 {
		encoded, err := json.Marshal(c.Body.Blobs)
		if err != nil {
			return fmt.Errorf("encoding blobs to json: %w", err)
		}
		encodedStr := string(encoded)
		bodyBlobs = &encodedStr
	}
	if c.ReplyTo != nil {
		replyToUri = &c.ReplyTo.Uri
		replyToCid = &c.ReplyTo.Cid
	}
	result, err := tx.Exec(
		// users can change the 'created' date.
		// skip update entirely if cid is unchanged.
		`insert into comments (
			did,
			collection,
			rkey,
			cid,
			subject_uri,
			subject_cid,
			body_text,
			body_original,
			body_blobs,
			created,
			reply_to_uri,
			reply_to_cid,
			pull_round_idx
		)
		values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		on conflict(did, collection, rkey)
		do update set
			cid            = excluded.cid,
			subject_uri    = excluded.subject_uri,
			subject_cid    = excluded.subject_cid,
			body_text      = excluded.body_text,
			body_original  = excluded.body_original,
			body_blobs     = excluded.body_blobs,
			created        = excluded.created,
			reply_to_uri   = excluded.reply_to_uri,
			reply_to_cid   = excluded.reply_to_cid,
			pull_round_idx = excluded.pull_round_idx,
			edited         = ?
		where comments.cid is not excluded.cid`,
		c.Did,
		c.Collection,
		c.Rkey,
		c.Cid,
		c.Subject.Uri,
		c.Subject.Cid,
		c.Body.Text,
		c.Body.Original,
		bodyBlobs,
		c.Created.Format(time.RFC3339),
		replyToUri,
		replyToCid,
		c.PullRoundIdx,
		time.Now().Format(time.RFC3339),
	)
	if err != nil {
		return err
	}

	c.Id, err = result.LastInsertId()
	if err != nil {
		return err
	}

	affected, err := result.RowsAffected()
	if err != nil {
		return err
	}

	if affected > 0 {
		// update references when comment is updated
		if err := putReferences(tx, c.AtUri(), references); err != nil {
			return fmt.Errorf("put reference_links: %w", err)
		}
	}

	return nil
}

// PurgeComments actually purges a comment row from db instead of marking it as "deleted"
func PurgeComments(e Execer, filters ...orm.Filter) error {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	_, err := e.Exec(fmt.Sprintf(`delete from comments %s`, whereClause), args...)
	return err
}

func DeleteComments(e Execer, filters ...orm.Filter) error {
	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	query := fmt.Sprintf(
		`update comments
		set body_text     = "",
			body_original = null,
			body_blobs    = null,
			deleted       = strftime('%%Y-%%m-%%dT%%H:%%M:%%SZ', 'now')
		%s`,
		whereClause,
	)

	_, err := e.Exec(query, args...)
	return err
}

func GetComments(e Execer, filters ...orm.Filter) ([]models.Comment, error) {
	var comments []models.Comment

	var conditions []string
	var args []any
	for _, filter := range filters {
		conditions = append(conditions, filter.Condition())
		args = append(args, filter.Arg()...)
	}

	whereClause := ""
	if conditions != nil {
		whereClause = " where " + strings.Join(conditions, " and ")
	}

	query := fmt.Sprintf(`
		select
			id,
			did,
			collection,
			rkey,
			cid,
			subject_uri,
			subject_cid,
			body_text,
			body_original,
			body_blobs,
			created,
			reply_to_uri,
			reply_to_cid,
			pull_round_idx,
			edited,
			deleted
		from
			comments
		%s
		`, whereClause)

	rows, err := e.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	for rows.Next() {
		var comment models.Comment
		var created string
		var cid, bodyBlobs, replyToUri, replyToCid, edited, deleted sql.Null[string]
		err := rows.Scan(
			&comment.Id,
			&comment.Did,
			&comment.Collection,
			&comment.Rkey,
			&cid,
			&comment.Subject.Uri,
			&comment.Subject.Cid,
			&comment.Body.Text,
			&comment.Body.Original,
			&bodyBlobs,
			&created,
			&replyToUri,
			&replyToCid,
			&comment.PullRoundIdx,
			&edited,
			&deleted,
		)
		if err != nil {
			return nil, err
		}

		if cid.Valid && cid.V != "" {
			comment.Cid = syntax.CID(cid.V)
		}

		if bodyBlobs.Valid && bodyBlobs.V != "" {
			if err := json.Unmarshal([]byte(bodyBlobs.V), &comment.Body.Blobs); err != nil {
				return nil, fmt.Errorf("decoding blobs: %w", err)
			}
		}

		if t, err := time.Parse(time.RFC3339, created); err == nil {
			comment.Created = t
		}

		if replyToUri.Valid && replyToCid.Valid {
			comment.ReplyTo = &atproto.RepoStrongRef{
				Uri: replyToUri.V,
				Cid: replyToCid.V,
			}
		}

		if edited.Valid {
			if t, err := time.Parse(time.RFC3339, edited.V); err == nil {
				comment.Edited = &t
			}
		}

		if deleted.Valid {
			if t, err := time.Parse(time.RFC3339, deleted.V); err == nil {
				comment.Deleted = &t
			}
		}

		comments = append(comments, comment)
	}

	if err := rows.Err(); err != nil {
		return nil, err
	}

	sort.Slice(comments, func(i, j int) bool {
		return comments[i].Created.Before(comments[j].Created)
	})

	return comments, nil
}
