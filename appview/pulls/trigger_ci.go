package pulls

import (
	"fmt"
	"net/http"
	"strings"

	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/models"
	"tangled.org/core/patchutil"
	"tangled.org/core/workflow"
)

func changedWorkflowFiles(patch string) ([]string, error) {
	files, err := patchutil.AsDiff(patch)
	if err != nil {
		return nil, err
	}

	var changed []string
	for _, f := range files {
		if f == nil {
			continue
		}
		for _, name := range []string{f.NewName, f.OldName} {
			if name != "" && strings.HasPrefix(name, workflow.WorkflowDir+"/") {
				changed = append(changed, name)
				break
			}
		}
	}
	return changed, nil
}

// TriggerCi manually triggers a CI pipeline for a fork-based pull request.
// authorized against and recorded under the target repo, but checked out
// from the fork at the latest round's commit.
func (s *Pulls) TriggerCi(w http.ResponseWriter, r *http.Request) {
	l := s.logger.With("handler", "TriggerCi")
	errorId := "pull-error"

	fail := func(msg string, err error) {
		if err != nil {
			l.Error(msg, "err", err)
		} else {
			l.Error(msg)
		}
		s.pages.Notice(w, errorId, msg)
	}

	f, err := s.repoResolver.Resolve(r)
	if err != nil {
		fail("failed to resolve repository", err)
		return
	}

	pull, ok := r.Context().Value("pull").(*models.Pull)
	if !ok {
		fail("failed to get pull", nil)
		return
	}
	l = l.With("pull_id", pull.PullId)

	if !pull.IsForkBased() {
		fail("this pull request is not fork-based", nil)
		return
	}

	if f.Spindle == "" {
		fail("this repository has no spindle configured", nil)
		return
	}

	latest := pull.LatestSubmission()
	if latest.SourceRev == "" {
		fail("cannot trigger ci: this round has no commit to run", nil)
		return
	}

	changedFiles, err := changedWorkflowFiles(latest.CombinedPatch())
	if err != nil {
		fail("failed to inspect the latest round's patch", err)
		return
	}
	if len(changedFiles) > 0 && r.URL.Query().Get("confirm") != "1" {
		fail(fmt.Sprintf("workflow files changed in this round (%s); review before running", strings.Join(changedFiles, ", ")), nil)
		return
	}

	forkRepo, err := db.GetRepoByDid(s.db, pull.PullSource.RepoDid.String())
	if err != nil {
		fail("failed to resolve the fork this pull request comes from", err)
		return
	}

	spindleClient, err := s.oauth.SpindleServiceClient(r, f.Spindle, tangled.CiTriggerPipelineNSID)
	if err != nil {
		fail("failed to authorize with spindle", err)
		return
	}

	pullAt := pull.AtUri().String()
	sourceBranch := pull.PullSource.Branch
	targetBranch := pull.TargetBranch
	out, err := tangled.CiTriggerPipeline(
		r.Context(),
		spindleClient,
		&tangled.CiTriggerPipeline_Input{
			Repo: f.RepoDid,
			Trigger: &tangled.CiTriggerPipeline_Input_Trigger{
				CiTrigger_PullRequest: &tangled.CiTrigger_PullRequest{
					Pull:         &pullAt,
					SourceBranch: &sourceBranch,
					SourceRepo:   &forkRepo.RepoDid,
					SourceSha:    latest.SourceRev,
					TargetBranch: targetBranch,
				},
			},
		},
	)
	if err != nil {
		fail("spindle rejected the trigger", err)
		return
	}
	l.Info("triggered ci for fork-based pull", "pipeline", out.Pipeline)

	user := s.oauth.GetMultiAccountUser(r)
	repoInfo := s.repoResolver.GetRepoInfo(r, user)
	dest := fmt.Sprintf("/%s/pulls/%d/round/%d", repoInfo.FullName(), pull.PullId, pull.LastRoundNumber())
	if r.Header.Get("HX-Request") == "true" {
		w.Header().Set("HX-Redirect", dest)
		w.WriteHeader(http.StatusOK)
		return
	}
	http.Redirect(w, r, dest, http.StatusSeeOther)
}
