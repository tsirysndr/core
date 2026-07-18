package logview

import (
	"bufio"
	"context"
	"io"
	"strings"

	"github.com/hpcloud/tail"
	"tangled.org/core/spindle/artifactstore"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/models"
)

// streams a workflow's log lines: tails the log file while running, reads
// the uploaded artifact once finished, same for every role. stop ends a
// live follow early, the channel closes when the source drains or ctx ends
func Follow(ctx context.Context, d *db.DB, reader artifactstore.Reader, logDir string, wid models.WorkflowId, finished bool) (<-chan *tail.Line, func(), error) {
	if finished && reader != nil && d != nil {
		if fl, err := d.GetFinishedLog(wid.Name); err == nil && fl.Ref != "" {
			rc, err := reader.Open(ctx, fl.Ref)
			if err == nil {
				ch := make(chan *tail.Line, 64)
				followCtx, cancel := context.WithCancel(ctx)
				go func() {
					defer close(ch)
					defer rc.Close()
					scanner := bufio.NewScanner(rc)
					for scanner.Scan() {
						select {
						case <-followCtx.Done():
							return
						case ch <- &tail.Line{Text: strings.TrimSuffix(scanner.Text(), "\r")}:
						}
					}
				}()
				return ch, cancel, nil
			}
		}
	}

	t, err := tail.TailFile(models.LogFilePath(logDir, wid), tail.Config{
		Follow:    !finished,
		ReOpen:    !finished,
		MustExist: false,
		Location:  &tail.SeekInfo{Offset: 0, Whence: io.SeekStart},
	})
	if err != nil {
		return nil, nil, err
	}
	return t.Lines, func() { _ = t.Stop() }, nil
}
