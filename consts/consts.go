package consts

const (
	TangledDid = "did:plc:wshs7t2adsemcrrd4snkeqli"
	IcyDid     = "did:plc:hwevmowznbiukdf6uk5dwrrq"

	DefaultSpindle = "spindle.tangled.sh"
	DefaultKnot    = "knot1.tangled.sh"
)

type Capability string

const (
	CapKnotACL      Capability = "knot-acl"
	CapRepoDidInput Capability = "repo-did-input"
)
