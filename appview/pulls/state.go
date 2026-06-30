package pulls

import (
	"net/http"
	"time"

	comatproto "github.com/bluesky-social/indigo/api/atproto"
	"github.com/bluesky-social/indigo/atproto/syntax"
	lexutil "github.com/bluesky-social/indigo/lex/util"
	"github.com/samber/lo"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/models"
	"tangled.org/core/tid"
)

func (s *Pulls) writePullStatusRecords(r *http.Request, actorDid string, subjects []syntax.ATURI, value models.StateValue) error {
	if len(subjects) == 0 {
		return nil
	}

	client, err := s.oauth.AuthorizedClient(r)
	if err != nil {
		return err
	}

	records, err := models.AsPullStatusRecords(subjects, value, time.Now())
	if err != nil {
		return err
	}

	writes := lo.Map(records, func(record tangled.RepoPullStatus, _ int) *comatproto.RepoApplyWrites_Input_Writes_Elem {
		rkey := tid.TID()
		return &comatproto.RepoApplyWrites_Input_Writes_Elem{
			RepoApplyWrites_Create: &comatproto.RepoApplyWrites_Create{
				Collection: tangled.RepoPullStatusNSID,
				Rkey:       &rkey,
				Value: &lexutil.LexiconTypeDecoder{
					Val: &record,
				},
			},
		}
	})

	_, err = comatproto.RepoApplyWrites(r.Context(), client, &comatproto.RepoApplyWrites_Input{
		Repo:   actorDid,
		Writes: writes,
	})
	return err
}
