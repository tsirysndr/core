package main

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

// HTTP metrics
var (
	requestsTotal = promauto.NewCounterVec(
		prometheus.CounterOpts{
			Name: "zoekt_dynamic_tangled_requests_total",
			Help: "Total number of HTTP requests by status code, method, and route.",
		},
		[]string{"method", "route", "code"},
	)
)
