package ssh

import (
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/ssh"
	wishtea "github.com/charmbracelet/wish/bubbletea"
	"tangled.org/core/appview/db"
	"tangled.org/core/orm"
)

func (s *Server) teaHandler(sess ssh.Session) (tea.Model, []tea.ProgramOption) {
	remote := sess.RemoteAddr().String()
	args := sess.Command()
	l := s.logger.With("remote", remote)
	l.Info("SSH connection", "args", args)
	defer l.Info("SSH connection closed", "remote", remote)

	renderer := wishtea.MakeRenderer(sess)

	if len(args) != 1 {
		l.Warn("bad invocation", "args", args)
		return newErrorModel(renderer, "usage: ssh -t <host> -p 2222 <at-uri>\nexample: ssh -t host -p 2222 at://did:web:knot.example/sh.tangled.pipeline/abc123"), wishtea.MakeOptions(sess)
	}

	rawURI := args[0]
	aturi, err := syntax.ParseATURI(rawURI)
	if err != nil {
		l.Warn("invalid AT URI", "uri", rawURI, "err", err)
		return newErrorModel(renderer, fmt.Sprintf("invalid AT URI %q: %v", rawURI, err)), wishtea.MakeOptions(sess)
	}

	did := aturi.Authority().String()
	const didWebPrefix = "did:web:"
	if len(did) <= len(didWebPrefix) {
		l.Warn("unsupported DID format", "did", did)
		return newErrorModel(renderer, fmt.Sprintf("unsupported DID format %q (expected did:web:...)", did)), wishtea.MakeOptions(sess)
	}
	knot := did[len(didWebPrefix):]
	rkey := aturi.RecordKey().String()

	l = l.With("knot", knot, "rkey", rkey)

	pipelines, err := db.GetPipelineStatuses(s.db, 1,
		orm.FilterEq("p.knot", knot),
		orm.FilterEq("p.rkey", rkey),
	)
	if err != nil || len(pipelines) == 0 {
		l.Warn("pipeline not found", "err", err)
		return newErrorModel(renderer, fmt.Sprintf("pipeline not found: %s", rawURI)), wishtea.MakeOptions(sess)
	}

	pipeline := pipelines[0]
	l.Info("serving pipeline", "workflows", len(pipeline.Statuses))
	pty, _, _ := sess.Pty()
	opts := append(wishtea.MakeOptions(sess), tea.WithAltScreen())
	return newPipelineModel(renderer, s, pipeline, pty.Window.Width, pty.Window.Height), opts
}
