package ssh

import (
	"fmt"

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

	if len(args) != 2 {
		l.Warn("bad invocation", "args", args)
		return newErrorModel(renderer, "usage: ssh -t -p <port> <host> <repoDID> <sha>"), wishtea.MakeOptions(sess)
	}

	repoDID := args[0]
	sha := args[1]

	l = l.With("repoDID", repoDID, "sha", sha)

	pipelines, err := db.GetPipelineStatuses(s.db, 1,
		orm.FilterEq("p.repo_did", repoDID),
		orm.FilterEq("p.sha", sha),
	)
	if err != nil || len(pipelines) == 0 {
		l.Warn("pipeline not found", "err", err)
		return newErrorModel(renderer, fmt.Sprintf("pipeline not found for repo %s @ %s", repoDID, sha)), wishtea.MakeOptions(sess)
	}

	pipeline := pipelines[0]
	l.Info("serving pipeline", "workflows", len(pipeline.Statuses))
	pty, _, _ := sess.Pty()
	opts := append(wishtea.MakeOptions(sess), tea.WithAltScreen())
	return newPipelineModel(renderer, s, pipeline, pty.Window.Width, pty.Window.Height), opts
}
