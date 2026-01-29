package knotmirror

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// Resync metrics
var (
	// TODO:
	// - working / waiting resycner counts
	resyncsStarted = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_resyncs_started_total",
		Help: "Total number of repo resyncs started",
	})
	resyncsCompleted = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_resyncs_completed_total",
		Help: "Total number of repo resyncs completed",
	})
	resyncsFailed = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_resyncs_failed_total",
		Help: "Total number of repo resyncs failed",
	})
	resyncDuration = promauto.NewHistogram(prometheus.HistogramOpts{
		Name:    "knotmirror_resync_duration_seconds",
		Help:    "Duration of repo resync operations",
		Buckets: prometheus.ExponentialBuckets(0.1, 2, 12),
	})
)
