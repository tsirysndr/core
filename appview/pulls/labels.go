package pulls

import (
	"context"
	"iter"
	"net/url"
	"slices"
	"time"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/orm"
	"tangled.org/core/tid"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/atclient"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
)

func (s *Pulls) pullLabelDefs(repo *models.Repo) (map[string]*models.LabelDefinition, error) {
	defs, err := db.GetLabelDefinitions(
		s.db,
		orm.FilterIn("at_uri", repo.Labels),
		orm.FilterContains("scope", tangled.RepoPullNSID),
	)
	if err != nil {
		return nil, err
	}

	out := make(map[string]*models.LabelDefinition, len(defs))
	for i := range defs {
		d := defs[i]
		if !slices.Contains(d.Scope, tangled.RepoPullNSID) {
			continue
		}
		out[d.AtUri().String()] = &d
	}
	return out, nil
}

func formLabelEntries(form url.Values, defs map[string]*models.LabelDefinition) iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for key := range defs {
			for _, v := range form[key] {
				if v == "" {
					continue
				}
				if !yield(key, v) {
					return
				}
			}
		}
	}
}

func labelStateFromForm(form url.Values, defs map[string]*models.LabelDefinition) models.LabelState {
	state := models.NewLabelState()
	actx := &models.LabelApplicationCtx{Defs: defs}
	for key, val := range formLabelEntries(form, defs) {
		_ = actx.ApplyLabelOp(state, models.LabelOp{
			Operation:    models.LabelOperationAdd,
			OperandKey:   key,
			OperandValue: val,
		})
	}
	return state
}

func buildCreationLabelOps(
	userDid syntax.DID,
	subject syntax.ATURI,
	rkey string,
	form url.Values,
	defs map[string]*models.LabelDefinition,
	performedAt time.Time,
) []models.LabelOp {
	var ops []models.LabelOp
	for key, val := range formLabelEntries(form, defs) {
		ops = append(ops, models.LabelOp{
			Did:          userDid.String(),
			Rkey:         rkey,
			Subject:      subject,
			Operation:    models.LabelOperationAdd,
			OperandKey:   key,
			OperandValue: val,
			PerformedAt:  performedAt,
		})
	}
	return ops
}

func (s *Pulls) applyCreationLabels(
	ctx context.Context,
	client *atclient.APIClient,
	userDid syntax.DID,
	pulls []*models.Pull,
	form url.Values,
	repo *models.Repo,
) {
	l := s.logger.With("handler", "applyCreationLabels", "user", userDid)

	defs, err := s.pullLabelDefs(repo)
	if err != nil {
		l.Warn("failed to fetch label defs", "err", err)
		return
	}
	if len(defs) == 0 {
		return
	}

	perCidForms := parseStackLabelForms(form)

	applyAll := form.Get("applyLabelsToAll") == "on"
	var firstStackForm url.Values
	if applyAll && len(pulls) > 0 && len(pulls[0].Submissions) > 0 {
		if firstCid := pulls[0].Submissions[0].ChangeId(); firstCid != "" {
			if f, ok := perCidForms[firstCid]; ok {
				firstStackForm = f
			}
		}
	}

	performedAt := time.Now()
	for _, pull := range pulls {
		labelForm := form
		if firstStackForm != nil {
			labelForm = firstStackForm
		} else if len(perCidForms) > 0 && len(pull.Submissions) > 0 {
			if cid := pull.Submissions[0].ChangeId(); cid != "" {
				if perForm, ok := perCidForms[cid]; ok {
					labelForm = perForm
				}
			}
		}
		rkey := tid.TID()
		raw := buildCreationLabelOps(userDid, pull.AtUri(), rkey, labelForm, defs, performedAt)

		valid := make([]models.LabelOp, 0, len(raw))
		for _, op := range raw {
			def := defs[op.OperandKey]

			// validate permissions: only collaborators can apply labels currently
			//
			// TODO: introduce a repo:triage permission
			ok, err := s.acl.HasRepoPermissionErr(ctx, repo, op.Did, "repo:push")
			if err != nil {
				l.Warn("invalid label op", "err", err, "subject", op.Subject, "key", op.OperandKey)
				continue
			}
			if !ok {
				l.Warn("forbidden label op", "subject", op.Subject, "key", op.OperandKey)
				continue
			}

			// resolve Handle to DID
			if def.ValueType.IsString() && def.ValueType.IsDidFormat() {
				val := syntax.AtIdentifier(op.OperandValue)
				if val.IsHandle() {
					ident, err := s.idResolver.Directory().Lookup(ctx, val)
					if err != nil {
						l.Warn("failed to resolve handle", "err", err, "subject", op.Subject, "key", op.OperandKey)
					}
					op.OperandValue = ident.DID.String()
				}
			}

			if err := def.ValidateOperandValue(&op); err != nil {
				l.Warn("invalid label op", "err", err, "subject", op.Subject, "key", op.OperandKey)
				continue
			}
			valid = append(valid, op)
		}
		if len(valid) == 0 {
			continue
		}

		record := models.LabelOpsAsRecord(valid)
		if _, err := comatproto.RepoPutRecord(ctx, client, &comatproto.RepoPutRecord_Input{
			Collection: tangled.LabelOpNSID,
			Repo:       userDid.String(),
			Rkey:       rkey,
			Record:     &lexutil.LexiconTypeDecoder{Val: &record},
		}); err != nil {
			l.Warn("failed to write label ops to PDS", "err", err, "subject", pull.AtUri())
			continue
		}

		if err := s.indexLabelOps(ctx, valid); err != nil {
			l.Warn("failed to index label ops", "err", err, "subject", pull.AtUri())
			if _, err := comatproto.RepoDeleteRecord(context.Background(), client, &comatproto.RepoDeleteRecord_Input{
				Collection: tangled.LabelOpNSID,
				Repo:       userDid.String(),
				Rkey:       rkey,
			}); err != nil {
				l.Warn("failed to rollback label ops record from PDS", "err", err, "subject", pull.AtUri())
			}
			continue
		}

		s.notifier.NewPullLabelOp(ctx, userDid, pull, valid)
	}
}

func (s *Pulls) indexLabelOps(ctx context.Context, ops []models.LabelOp) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	for _, op := range ops {
		if _, err := db.AddLabelOp(tx, &op); err != nil {
			return err
		}
	}
	return tx.Commit()
}
