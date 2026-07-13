package ssh

import (
	"fmt"

	"github.com/bluesky-social/indigo/atproto/syntax"
	indigoxrpc "github.com/bluesky-social/indigo/xrpc"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/ssh"
	wishtea "github.com/charmbracelet/wish/bubbletea"
	"tangled.org/core/api/tangled"
	"tangled.org/core/appview/db"
	"tangled.org/core/hostutil"
	extlexutil "tangled.org/core/lexutil"
	"tangled.org/core/orm"
)

func (s *Server) teaHandler(sess ssh.Session) (tea.Model, []tea.ProgramOption) {
	remote := sess.RemoteAddr().String()
	args := sess.Command()
	l := s.logger.With("remote", remote)
	l.Info("SSH connection", "args", args)
	defer l.Info("SSH connection closed", "remote", remote)

	renderer := wishtea.MakeRenderer(sess)

	if len(args) != 2 {
		l.Warn("bad invocation", "args", args)
		return newErrorModel(renderer, "usage: ssh -t -p <port> <host> <repoDID> <sha>"), wishtea.MakeOptions(sess)
	}

	repoDID := args[0]
	sha := args[1]

	l = l.With("repoDID", repoDID, "sha", sha)

	repo, err := db.GetRepo(s.db, orm.FilterEq("repo_did", repoDID))
	if err != nil {
		l.Warn("repo not found", "err", err)
		return newErrorModel(renderer, fmt.Sprintf("repo %s not found", repoDID)), wishtea.MakeOptions(sess)
	}
	if repo.Spindle == "" {
		l.Warn("no spindle configured")
		return newErrorModel(renderer, "no spindle configured for this repo"), wishtea.MakeOptions(sess)
	}

	l = l.With("spindle", repo.Spindle)

	host, err := hostutil.EnsureHttpScheme(repo.Spindle)
	if err != nil {
		l.Warn("invalid spindlie hostname", "err", err)
		return newErrorModel(renderer, fmt.Sprintf("invalid spindle host %q", repo.Spindle)), wishtea.MakeOptions(sess)
	}

	xrpcc := extlexutil.Client{Client: indigoxrpc.Client{Host: host}}
	out, err := tangled.CiQueryPipelines(sess.Context(), &xrpcc, []string{sha}, "", nil, 1, repoDID)
	if err != nil || len(out.Pipelines) == 0 {
		l.Warn("pipeline not found", "err", err)
		return newErrorModel(renderer, fmt.Sprintf("pipeline not found for repo %s @ %s", repoDID, sha)), wishtea.MakeOptions(sess)
	}

	pipeline := out.Pipelines[0]
	if _, err := syntax.ParseTID(pipeline.Id); err != nil {
		l.Warn("invalid pipeline id", "id", pipeline.Id, "err", err)
		return newErrorModel(renderer, fmt.Sprintf("invalid pipeline id %q", pipeline.Id)), wishtea.MakeOptions(sess)
	}

	l.Info("serving pipeline", "pipeline", pipeline.Id, "workflows", len(pipeline.Workflows))
	pty, _, _ := sess.Pty()
	opts := append(wishtea.MakeOptions(sess), tea.WithAltScreen())
	return newPipelineModel(renderer, &xrpcc, pipeline, pty.Window.Width, pty.Window.Height), opts
}
