package knotstream

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// KnotStream metrics
var (
	knotstreamEventsReceived = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_knotstream_events_received_total",
		Help: "Total number of events received from knotstream",
	})
	knotstreamEventsProcessed = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_knotstream_events_processed_total",
		Help: "Total number of events successfully processed",
	})
	knotstreamEventsSkipped = promauto.NewCounter(prometheus.CounterOpts{
		Name: "knotmirror_knotstream_events_skipped_total",
		Help: "Total number of events skipped (not tracked)",
	})
)

// slurper metrics
var connectedInbound = promauto.NewGauge(prometheus.GaugeOpts{
	Name: "knotmirror_connected_inbound",
	Help: "Number of inbound knotstream we are consuming",
})
