package xrpc

import (
	"fmt"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

var (
	httpRequestsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "knotmirror_http_requests_total",
		Help: "Total number of HTTP requests",
	}, []string{"method", "path", "status", "repo"})

	httpRequestDuration = promauto.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "knotmirror_http_request_duration_seconds",
		Help:    "HTTP request duration in seconds",
		Buckets: prometheus.DefBuckets,
	}, []string{"method", "path", "status", "repo"})
)

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (r *statusRecorder) WriteHeader(status int) {
	r.status = status
	r.ResponseWriter.WriteHeader(status)
}

func metricsMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		start := time.Now()

		next.ServeHTTP(rec, r)

		routePattern := chi.RouteContext(r.Context()).RoutePattern()
		if routePattern == "" {
			routePattern = "unknown"
		}

		repo := r.URL.Query().Get("repo")
		if repo == "" {
			repo = "unknown"
		}

		status := fmt.Sprintf("%d", rec.status)
		duration := time.Since(start).Seconds()

		httpRequestsTotal.WithLabelValues(r.Method, routePattern, status, repo).Inc()
		httpRequestDuration.WithLabelValues(r.Method, routePattern, status, repo).Observe(duration)
	})
}
