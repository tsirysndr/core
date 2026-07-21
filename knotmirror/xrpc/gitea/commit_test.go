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
` + " " + `
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
-----END PGP SIGNATURE-----`, commitFromReader.PGPSignature)
	assert.Equal(t, `tree f1a6cb52b2d16773290cefe49ad0684b50a4f930
parent 37991dec2c8e592043f47155ce4808d4580f9123
author silverwind <me@silverwind.io> 1563741793 +0200
committer silverwind <me@silverwind.io> 1563741793 +0200

empty commit`, tcommit.Payload())
	assert.Equal(t, "silverwind <me@silverwind.io>", commitFromReader.Author.String())
}

func TestCommitFromReaderMergeTag(t *testing.T) {
	// Built with explicit "\n" concatenation, not a backtick literal: the blank
	// mergetag continuation lines are " \n" (space + newline) and gofmt/editors
	// strip trailing whitespace from raw literals, which would corrupt the input.
	commitString := "tree 635dfb8e1e9d4d75855cc23eb28d35533f55b42f\n" +
		"parent c1fa0bb633e4a6b11e83ffc57fa5abe8ebb87891\n" +
		"parent 8f80b5b227ef9ea422080487715c841856339aed\n" +
		"author Linus Torvalds <torvalds@linux-foundation.org> 1778539129 -0700\n" +
		"committer Linus Torvalds <torvalds@linux-foundation.org> 1778539129 -0700\n" +
		"mergetag object 8f80b5b227ef9ea422080487715c841856339aed\n" +
		" type commit\n" +
		" tag linux_kselftest-kunit-fixes-7.1-rc4\n" +
		" tagger Shuah Khan <skhan@linuxfoundation.org> 1778535878 -0600\n" +
		" \n" + // blank continuation line inside the mergetag: the bug trigger
		" linux_kselftest-kunit-fixes-7.1-rc4\n" +
		" \n" +
		" Fix to decouple KUNIT_DEBUGFS and KUNIT_ALL_TESTS options.\n" +
		" -----BEGIN PGP SIGNATURE-----\n" +
		" \n" +
		" iQIzBAABCgAdFiEEPZKym/RZuOCGeA/kCwJExA0NQxwFAmoCUf0ACgkQCwJExA0N\n" +
		" =QdSk\n" +
		" -----END PGP SIGNATURE-----\n" +
		"\n" + // real header/message separator: truly empty line
		"Merge tag 'linux_kselftest-kunit-fixes-7.1-rc4' of git://example\n" +
		"\n" +
		"Pull kunit fixes from Shuah Khan"

	sha := plumbing.NewHash("50897c955902c93ae71c38698abb910525ebdc89")

	c, err := ReadCommit(sha, strings.NewReader(commitString))
	require.NoError(t, err)
	require.NotNil(t, c)

	// Message must be only the merge message, not polluted with the tag body/signature.
	assert.Equal(t, "Merge tag 'linux_kselftest-kunit-fixes-7.1-rc4' of git://example\n\nPull kunit fixes from Shuah Khan", c.Message)

	// mergetag is captured, and this commit itself is not gpg-signed.
	assert.Contains(t, c.MergeTag, "type commit\n")
	assert.Contains(t, c.MergeTag, "-----END PGP SIGNATURE-----")
	assert.Empty(t, c.PGPSignature)

	// The broken parser dumped stray continuation lines into ExtraHeaders[""].
	assert.NotContains(t, c.ExtraHeaders, "")

	assert.Equal(t, "Linus Torvalds <torvalds@linux-foundation.org>", c.Author.String())
}
