package validator

import (
	"tangled.org/core/appview/db"
	"tangled.org/core/appview/knotacl"
	"tangled.org/core/appview/pages/markup"
	"tangled.org/core/idresolver"
)

type Validator struct {
	db        *db.DB
	sanitizer markup.Sanitizer
	resolver  *idresolver.Resolver
	acl       *knotacl.Service
}

func New(db *db.DB, res *idresolver.Resolver, acl *knotacl.Service) *Validator {
	return &Validator{
		db:        db,
		sanitizer: markup.NewSanitizer(),
		resolver:  res,
		acl:       acl,
	}
}
