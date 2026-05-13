package oauth

import "regexp"

var (
	permanentAuthErrorRe  = regexp.MustCompile(`\b(invalid_grant|invalid_client|unauthorized_client)\b`)
	staleAccessTokenErrRe = regexp.MustCompile(`HTTP 401\b.*(AuthenticationRequired|Invalid OAuth access token|invalid_token)`)
)

func IsPermanentAuthErr(err error) bool {
	if err == nil {
		return false
	}
	return permanentAuthErrorRe.MatchString(err.Error())
}

func IsStaleAccessTokenErr(err error) bool {
	if err == nil {
		return false
	}
	return staleAccessTokenErrRe.MatchString(err.Error())
}
