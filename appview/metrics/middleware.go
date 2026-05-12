package metrics

import (
	"bufio"
	"fmt"
	"net"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"
)

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (r *statusRecorder) WriteHeader(status int) {
	r.status = status
	r.ResponseWriter.WriteHeader(status)
}

func (r *statusRecorder) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	h, ok := r.ResponseWriter.(http.Hijacker)
	if !ok {
		return nil, nil, fmt.Errorf("underlying ResponseWriter does not support hijacking")
	}
	return h.Hijack()
}

func Middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		start := time.Now()

		next.ServeHTTP(rec, r)

		// use the matched route pattern to avoid high cardinality
		routePattern := chi.RouteContext(r.Context()).RoutePattern()
		if routePattern == "" {
			routePattern = "unknown"
		}

		status := fmt.Sprintf("%d", rec.status)
		duration := time.Since(start).Seconds()

		HttpRequestsTotal.WithLabelValues(r.Method, routePattern, status).Inc()
		HttpRequestDuration.WithLabelValues(r.Method, routePattern, status).Observe(duration)
	})
}
