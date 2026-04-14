package models

import (
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
)

type SiteDeployStatus string

const (
	SiteDeployStatusSuccess SiteDeployStatus = "success"
	SiteDeployStatusFailure SiteDeployStatus = "failure"
)

type SiteDeployTrigger string

const (
	SiteDeployTriggerConfigChange SiteDeployTrigger = "config_change"
	SiteDeployTriggerPush         SiteDeployTrigger = "push"
)

func (t SiteDeployTrigger) Label() string {
	switch t {
	case SiteDeployTriggerConfigChange:
		return "config change"
	case SiteDeployTriggerPush:
		return "push"
	default:
		return string(t)
	}
}

type SiteDeploy struct {
	Id        int64
	RepoDid   syntax.DID
	Branch    string
	Dir       string
	CommitSHA string
	Status    SiteDeployStatus
	Trigger   SiteDeployTrigger
	Error     string
	CreatedAt time.Time
}
