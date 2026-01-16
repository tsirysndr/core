package models

type Validator interface {
	// Validate checks the object and returns any error.
	Validate() error
}
