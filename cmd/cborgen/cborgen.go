package main

import (
	cbg "github.com/whyrusleeping/cbor-gen"
	"tangled.org/core/api/tangled"
)

func main() {

	genCfg := cbg.Gen{
		MaxStringLength: 1_000_000,
	}

	if err := genCfg.WriteMapEncodersToFile(
		"api/tangled/cbor_gen.go",
		"tangled",
		tangled.ActorProfile{},
		tangled.CiPipeline{},
		tangled.CiPipeline_Trigger{},
		tangled.CiPipeline_Workflow{},
		tangled.CiSubscribePipelineLogs_Control{},
		tangled.CiSubscribePipelineLogs_Data{},
		tangled.CiTrigger_Manual{},
		tangled.CiTrigger_Pair{},
		tangled.CiTrigger_PullRequest{},
		tangled.CiTrigger_Push{},
		tangled.FeedComment{},
		tangled.FeedReaction{},
		tangled.FeedStar{},
		tangled.FeedStar_Repo{},
		tangled.FeedStar_String{},
		tangled.GitRefUpdate{},
		tangled.GitRefUpdate_CommitCountBreakdown{},
		tangled.GitRefUpdate_IndividualEmailCommitCount{},
		tangled.GitRefUpdate_IndividualLanguageSize{},
		tangled.GitRefUpdate_LangBreakdown{},
		tangled.GitRefUpdate_Meta{},
		tangled.GraphFollow{},
		tangled.GraphVouch{},
		tangled.Knot{},
		tangled.KnotMember{},
		tangled.LabelDefinition{},
		tangled.LabelDefinition_ValueType{},
		tangled.LabelOp{},
		tangled.LabelOp_Operand{},
		tangled.MarkupMarkdown{},
		tangled.Pipeline{},
		tangled.Pipeline_CloneOpts{},
		tangled.Pipeline_ManualTriggerData{},
		tangled.Pipeline_Pair{},
		tangled.Pipeline_PullRequestTriggerData{},
		tangled.Pipeline_PushTriggerData{},
		tangled.PipelineStatus{},
		tangled.Pipeline_TriggerMetadata{},
		tangled.Pipeline_TriggerRepo{},
		tangled.Pipeline_Workflow{},
		tangled.PublicKey{},
		tangled.Repo{},
		tangled.RepoArtifact{},
		tangled.RepoCollaborator{},
		tangled.RepoIssue{},
		tangled.RepoIssueComment{},
		tangled.RepoIssueState{},
		tangled.RepoPull{},
		tangled.RepoPullComment{},
		tangled.RepoPull_Round{},
		tangled.RepoPull_Source{},
		tangled.RepoPullStatus{},
		tangled.RepoPull_Target{},
		tangled.Spindle{},
		tangled.SpindleMember{},
		tangled.String{},
	); err != nil {
		panic(err)
	}

}
