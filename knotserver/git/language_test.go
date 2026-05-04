package git

import (
	"context"
	"strings"
	"testing"

	"github.com/stretchr/testify/require"
	"github.com/stretchr/testify/suite"
)

type LanguageSuite struct {
	suite.Suite
	*RepoSuite
}

func TestLanguageSuite(t *testing.T) {
	t.Parallel()
	suite.Run(t, new(LanguageSuite))
}

func (s *LanguageSuite) SetupTest() {
	s.RepoSuite = NewRepoSuite(s.T())
}

func (s *LanguageSuite) TearDownTest() {
	s.RepoSuite.cleanup()
}

func (s *LanguageSuite) TestAnalyzeLanguagesSkipsUnknownGodotMetadata() {
	s.init()

	gdscript := "extends CharacterBody3D\n\nfunc _ready():\n\tpass\n"
	s.commitFile("code/player.gd", gdscript, "Add GDScript")
	s.commitFile("code/player.gd.uid", "uid://b5gy7avoc3cs1\n", "Add Godot uid")
	s.commitFile(
		"assets/player.glb.import",
		"[remap]\n\nimporter=\"scene\"\n\n[params]\n"+strings.Repeat("animation/import=true\n", 512),
		"Add Godot import metadata",
	)
	s.commitFile(
		"scenes/player.tscn",
		"[gd_scene format=4]\n\n[sub_resource type=\"ArrayMesh\"]\n"+strings.Repeat("vertex_data = PackedByteArray(\"AAAA\")\n", 512),
		"Add Godot scene",
	)

	gr, err := Open(s.repo.path, "")
	require.NoError(s.T(), err)

	langs, err := gr.AnalyzeLanguages(context.Background())
	require.NoError(s.T(), err)

	require.NotContains(s.T(), langs, "")
	require.Equal(s.T(), LangBreakdown{
		"GDScript": int64(len(gdscript)),
	}, langs)
}
