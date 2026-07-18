package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"log/slog"
	"os"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/urfave/cli/v3"
	tlog "tangled.org/core/log"
	"tangled.org/core/spindle"
	"tangled.org/core/spindle/db"
	"tangled.org/core/spindle/mill"
)

func main() {
	cmd := &cli.Command{
		Name:  "spindle",
		Usage: "spindle continuous integration runner",
		Commands: []*cli.Command{
			Command(),
			millCommand(),
			executorCommand(),
		},
		DefaultCommand: "run",
	}

	logger := tlog.New("spindle")
	slog.SetDefault(logger)

	ctx := context.Background()
	ctx = tlog.IntoContext(ctx, logger)

	if err := cmd.Run(ctx, os.Args); err != nil {
		logger.Error(err.Error())
		os.Exit(-1)
	}
}

func Command() *cli.Command {
	return &cli.Command{
		Name:  "run",
		Usage: "run the spindle server",
		Action: func(ctx context.Context, cmd *cli.Command) error {
			return spindle.Run(ctx)
		},
	}
}

var dbFlag = &cli.StringFlag{
	Name:    "db",
	Usage:   "path to the spindle sqlite db",
	Value:   "spindle.db",
	Sources: cli.EnvVars("SPINDLE_SERVER_DB_PATH"),
}

func openDB(ctx context.Context, cmd *cli.Command) (*db.DB, error) {
	return db.Make(ctx, cmd.String("db"))
}

// lets executor operators reset stream state
func executorCommand() *cli.Command {
	return &cli.Command{
		Name:  "executor",
		Usage: "executor host administration",
		Commands: []*cli.Command{
			{
				Name:  "reset-stream",
				Usage: "wipe the outbox and start a fresh stream epoch, for when the mill has lost this executor's acked position; restart the executor service afterwards",
				Flags: []cli.Flag{dbFlag},
				Action: func(ctx context.Context, cmd *cli.Command) error {
					d, err := openDB(ctx, cmd)
					if err != nil {
						return err
					}
					var b [8]byte
					if _, err := rand.Read(b[:]); err != nil {
						return err
					}
					epoch := hex.EncodeToString(b[:])
					if err := d.SetOutboxEpoch(epoch); err != nil {
						return fmt.Errorf("resetting outbox: %w", err)
					}
					// clear artifacts waiting on leases from the old
					// stream, replaying them under a new epoch would
					// trip the mill's lease-epoch check
					if err := d.ClearPendingArtifacts(); err != nil {
						return fmt.Errorf("clearing pending artifacts: %w", err)
					}
					fmt.Printf("outbox wiped; new stream epoch %s. restart the executor to reconnect cleanly\n", epoch)
					return nil
				},
			},
		},
	}
}

// lets mill operators manage executors
func millCommand() *cli.Command {
	return &cli.Command{
		Name:  "mill",
		Usage: "mill host administration",
		Commands: []*cli.Command{
			{
				Name:  "executor",
				Usage: "manage executors allowed to join this mill",
				Commands: []*cli.Command{
					{
						Name:      "add",
						Usage:     "register an executor and print its token",
						ArgsUsage: "<name>",
						Flags: []cli.Flag{
							dbFlag,
							&cli.DurationFlag{
								Name:  "ttl",
								Usage: "token lifetime (e.g. 720h); omit for no expiry",
							},
							&cli.StringSliceFlag{
								Name:  "label",
								Usage: "authorized labels for this executor",
							},
						},
						Action: func(ctx context.Context, cmd *cli.Command) error {
							name := cmd.Args().First()
							if name == "" {
								return fmt.Errorf("usage: spindle mill executor add <name>")
							}
							d, err := openDB(ctx, cmd)
							if err != nil {
								return err
							}
							token, err := mill.GenerateToken()
							if err != nil {
								return err
							}
							var expiresAt *time.Time
							if ttl := cmd.Duration("ttl"); ttl > 0 {
								exp := time.Now().UTC().Add(ttl)
								expiresAt = &exp
							}
							labels := cmd.StringSlice("label")
							if err := d.AddExecutorToken(name, mill.HashToken(token), expiresAt, labels); err != nil {
								return fmt.Errorf("registering executor %q: %w", name, err)
							}
							fmt.Println(token)
							return nil
						},
					},
					{
						Name:  "list",
						Usage: "list registered executors",
						Flags: []cli.Flag{dbFlag},
						Action: func(ctx context.Context, cmd *cli.Command) error {
							d, err := openDB(ctx, cmd)
							if err != nil {
								return err
							}
							tokens, err := d.ListExecutorTokens()
							if err != nil {
								return err
							}
							w := tabwriter.NewWriter(os.Stdout, 0, 0, 3, ' ', 0)
							fmt.Fprintln(w, "NAME\tCREATED\tEXPIRES\tLABELS\tQUARANTINE")
							for _, t := range tokens {
								expires := "never"
								if t.ExpiresAt != nil {
									expires = t.ExpiresAt.Format(time.RFC3339)
									if time.Now().After(*t.ExpiresAt) {
										expires += " (expired)"
									}
								}
								labels := strings.Join(t.Labels, ",")
								if labels == "" {
									labels = "-"
								}
								quarantine := "-"
								if t.QuarantineReason != nil {
									quarantine = *t.QuarantineReason
									if t.QuarantinedAt != nil {
										quarantine = *t.QuarantinedAt + ": " + quarantine
									}
								}
								fmt.Fprintf(w, "%s\t%s\t%s\t%s\t%s\n", t.Name, t.CreatedAt, expires, labels, quarantine)
							}
							return w.Flush()
						},
					},
					{
						Name:      "unquarantine",
						Usage:     "allow a quarantined executor to reconnect",
						ArgsUsage: "<name>",
						Flags:     []cli.Flag{dbFlag},
						Action: func(ctx context.Context, cmd *cli.Command) error {
							name := cmd.Args().First()
							if name == "" {
								return fmt.Errorf("usage: spindle mill executor unquarantine <name>")
							}
							d, err := openDB(ctx, cmd)
							if err != nil {
								return err
							}
							ok, err := d.ClearExecutorQuarantine(name)
							if err != nil {
								return err
							}
							if !ok {
								return fmt.Errorf("no such executor identity %q", name)
							}
							return nil
						},
					},
					{
						Name:      "reset-cursor",
						Usage:     "reset the mill's acked stream position for an executor after mill state loss; takes effect on the executor's next reconnect",
						ArgsUsage: "<name>",
						Flags: []cli.Flag{
							dbFlag,
							&cli.UintFlag{
								Name: "to",
								Usage: "skip forward to this acked seqno instead of forgetting everything; " +
									"events at or below it are treated as applied (accepts their loss)",
							},
						},
						Action: func(ctx context.Context, cmd *cli.Command) error {
							name := cmd.Args().First()
							if name == "" {
								return fmt.Errorf("usage: spindle mill executor reset-cursor <name> [--to <seqno>]")
							}
							d, err := openDB(ctx, cmd)
							if err != nil {
								return err
							}
							if cmd.IsSet("to") {
								n, err := d.SetExecutorCursors(name, uint64(cmd.Uint("to")))
								if err != nil {
									return err
								}
								if n == 0 {
									return fmt.Errorf("no cursor rows for executor %q", name)
								}
								fmt.Printf("skipped %s forward to seqno %d (%d epoch rows); earlier events are lost\n", name, cmd.Uint("to"), n)
								return nil
							}
							n, err := d.DeleteExecutorCursors(name)
							if err != nil {
								return err
							}
							fmt.Printf("forgot %d cursor row(s) for %s; the mill now expects its stream from seqno 1\n", n, name)
							return nil
						},
					},
					{
						Name:      "revoke",
						Usage:     "revoke an executor's token",
						ArgsUsage: "<name>",
						Flags:     []cli.Flag{dbFlag},
						Action: func(ctx context.Context, cmd *cli.Command) error {
							name := cmd.Args().First()
							if name == "" {
								return fmt.Errorf("usage: spindle mill executor revoke <name>")
							}
							d, err := openDB(ctx, cmd)
							if err != nil {
								return err
							}
							ok, err := d.RevokeExecutorToken(name)
							if err != nil {
								return err
							}
							if !ok {
								return fmt.Errorf("no such executor identity %q", name)
							}
							return nil
						},
					},
				},
			},
		},
	}
}
