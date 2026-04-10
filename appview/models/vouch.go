package models

import (
	"fmt"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/ipfs/go-cid"
)

type VouchSuggestion struct {
	Did               syntax.DID
	Reason            string
	VouchRelationship *VouchRelationship
}

type VouchKind string

const (
	VouchKindVouch    VouchKind = "vouch"
	VouchKindDenounce VouchKind = "denounce"
)

func ParseVouchKind(v string) (VouchKind, error) {
	switch v {
	case "vouch":
		return VouchKindVouch, nil
	case "denounce":
		return VouchKindDenounce, nil
	default:
		return VouchKindVouch, fmt.Errorf("invalid vouch kind: %s", v)
	}
}

type Vouch struct {
	Did        syntax.DID
	SubjectDid syntax.DID
	Cid        cid.Cid
	Kind       VouchKind
	Reason     *string
	CreatedAt  time.Time
}

func (v Vouch) IsVouch() bool {
	return v.Kind == VouchKindVouch
}

func (v Vouch) IsDenounce() bool {
	return v.Kind == VouchKindDenounce
}

type VouchStats struct {
	Vouches   int64
	Denounces int64
}

type VouchRelationship struct {
	ViewerDid  syntax.DID
	SubjectDid syntax.DID

	NetworkVouches []Vouch
}

func (vr *VouchRelationship) IsDirectVouch() bool {
	for _, v := range vr.NetworkVouches {
		if v.Did == vr.ViewerDid && v.SubjectDid == vr.SubjectDid && v.Kind == VouchKindVouch {
			return true
		}
	}
	return false
}

func (vr *VouchRelationship) IsDirectDenounce() bool {
	for _, v := range vr.NetworkVouches {
		if v.Did == vr.ViewerDid && v.SubjectDid == vr.SubjectDid && v.Kind == VouchKindDenounce {
			return true
		}
	}
	return false
}

func (vr *VouchRelationship) IndirectVouches() []Vouch {
	var indirectVouches []Vouch
	for _, v := range vr.NetworkVouches {
		if v.Did != vr.ViewerDid {
			indirectVouches = append(indirectVouches, v)
		}
	}
	return indirectVouches
}

func (vr *VouchRelationship) IsEmpty() bool {
	return len(vr.NetworkVouches) == 0
}

func (vr *VouchRelationship) GetDirectVouch() *Vouch {
	for _, v := range vr.NetworkVouches {
		if v.Did == vr.ViewerDid && v.SubjectDid == vr.SubjectDid {
			return &v
		}
	}
	return nil
}

func (vr *VouchRelationship) VouchStrength() int {
	count := 0
	for _, v := range vr.NetworkVouches {
		if v.Did != vr.ViewerDid && v.Kind == VouchKindVouch {
			count++
		}
	}
	return count
}

func (vr *VouchRelationship) DenounceStrength() int {
	count := 0
	for _, v := range vr.NetworkVouches {
		if v.Did != vr.ViewerDid && v.Kind == VouchKindDenounce {
			count++
		}
	}
	return count
}
