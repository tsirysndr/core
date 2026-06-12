// Copyright 2021 The Gitea Authors. All rights reserved.
// SPDX-License-Identifier: MIT

package gitea

import (
	"strings"
	"testing"

	"github.com/go-git/go-git/v5/plumbing"
	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
	"tangled.org/core/types"
)

func TestCommitFromReader(t *testing.T) {
	commitString := `tree f1a6cb52b2d16773290cefe49ad0684b50a4f930
parent 37991dec2c8e592043f47155ce4808d4580f9123
author silverwind <me@silverwind.io> 1563741793 +0200
committer silverwind <me@silverwind.io> 1563741793 +0200
gpgsig -----BEGIN PGP SIGNATURE-----
 iQIzBAABCAAdFiEEWPb2jX6FS2mqyJRQLmK0HJOGlEMFAl00zmEACgkQLmK0HJOG
 lEMDFBAAhQKKqLD1VICygJMEB8t1gBmNLgvziOLfpX4KPWdPtBk3v/QJ7OrfMrVK
 xlC4ZZyx6yMm1Q7GzmuWykmZQJ9HMaHJ49KAbh5MMjjV/+OoQw9coIdo8nagRUld
 vX8QHzNZ6Agx77xHuDJZgdHKpQK3TrMDsxzoYYMvlqoLJIDXE1Sp7KYNy12nhdRg
 R6NXNmW8oMZuxglkmUwayMiPS+N4zNYqv0CXYzlEqCOgq9MJUcAMHt+KpiST+sm6
 FWkJ9D+biNPyQ9QKf1AE4BdZia4lHfPYU/C/DEL/a5xQuuop/zMQZoGaIA4p2zGQ
 /maqYxEIM/yRBQpT1jlODKPJrMEgx7SgY2hRU47YZ4fj6350fb6fNBtiiMAfJbjL
 S3Gh85E9fm3hJaNSPKAaJFYL1Ya2svuWfgHj677C56UcmYis7fhiiy1aJuYdHnSm
 sD53z/f0J+We4VZjY+pidvA9BGZPFVdR3wd3xGs8/oH6UWaLJAMGkLG6dDb3qDLm
 1LFZwsX8sdD32i1SiWanYQYSYMyFWr0awi4xdoMtYCL7uKBYtwtPyvq3cj4IrJlb
 mfeFhT57UbE4qukTDIQ0Y0WM40UYRTakRaDY7ubhXgLgx09Cnp9XTVMsHgT6j9/i
 1pxsB104XLWjQHTjr1JtiaBQEwFh9r2OKTcpvaLcbNtYpo7CzOs=
 =FRsO
 -----END PGP SIGNATURE-----

empty commit`

	sha := plumbing.NewHash("feaf4ba6bc635fec442f46ddd4512416ec43c2c2")

	commitFromReader, err := ReadCommit(sha, strings.NewReader(commitString))
	assert.NoError(t, err)
	require.NotNil(t, commitFromReader)

	tcommit := types.Commit{}
	tcommit.FromGoGitCommit(commitFromReader)
	assert.EqualValues(t, sha, tcommit.Hash)

	assert.Equal(t, `-----BEGIN PGP SIGNATURE-----

iQIzBAABCAAdFiEEWPb2jX6FS2mqyJRQLmK0HJOGlEMFAl00zmEACgkQLmK0HJOG
lEMDFBAAhQKKqLD1VICygJMEB8t1gBmNLgvziOLfpX4KPWdPtBk3v/QJ7OrfMrVK
xlC4ZZyx6yMm1Q7GzmuWykmZQJ9HMaHJ49KAbh5MMjjV/+OoQw9coIdo8nagRUld
vX8QHzNZ6Agx77xHuDJZgdHKpQK3TrMDsxzoYYMvlqoLJIDXE1Sp7KYNy12nhdRg
R6NXNmW8oMZuxglkmUwayMiPS+N4zNYqv0CXYzlEqCOgq9MJUcAMHt+KpiST+sm6
FWkJ9D+biNPyQ9QKf1AE4BdZia4lHfPYU/C/DEL/a5xQuuop/zMQZoGaIA4p2zGQ
/maqYxEIM/yRBQpT1jlODKPJrMEgx7SgY2hRU47YZ4fj6350fb6fNBtiiMAfJbjL
S3Gh85E9fm3hJaNSPKAaJFYL1Ya2svuWfgHj677C56UcmYis7fhiiy1aJuYdHnSm
sD53z/f0J+We4VZjY+pidvA9BGZPFVdR3wd3xGs8/oH6UWaLJAMGkLG6dDb3qDLm
1LFZwsX8sdD32i1SiWanYQYSYMyFWr0awi4xdoMtYCL7uKBYtwtPyvq3cj4IrJlb
mfeFhT57UbE4qukTDIQ0Y0WM40UYRTakRaDY7ubhXgLgx09Cnp9XTVMsHgT6j9/i
1pxsB104XLWjQHTjr1JtiaBQEwFh9r2OKTcpvaLcbNtYpo7CzOs=
=FRsO
-----END PGP SIGNATURE-----
`, commitFromReader.PGPSignature)
	assert.Equal(t, `tree f1a6cb52b2d16773290cefe49ad0684b50a4f930
parent 37991dec2c8e592043f47155ce4808d4580f9123
author silverwind <me@silverwind.io> 1563741793 +0200
committer silverwind <me@silverwind.io> 1563741793 +0200

empty commit`, tcommit.Payload())
	assert.Equal(t, "silverwind <me@silverwind.io>", commitFromReader.Author.String())
}
