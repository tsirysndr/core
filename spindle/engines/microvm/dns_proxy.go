package microvm

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"sync"
	"time"

	"github.com/miekg/dns"
)

const (
	dnsProxyIOTimeout       = 10 * time.Second
	dnsProxyIdleTimeout     = 30 * time.Second
	dnsProxyShutdownTimeout = 10 * time.Second
	dnsProxyMaxConnections  = 64
	dnsProxyMaxTCPQueries   = 128
	dnsProxyResolvConfPath  = "/etc/resolv.conf"
)

type DNSProxy struct {
	port uint32
	srv  *dns.Server

	closeOnce sync.Once
	closeErr  error
}

func StartDNSProxy(ctx context.Context, cid uint32, logger *slog.Logger) (*DNSProxy, error) {
	if ctx == nil {
		ctx = context.Background()
	}

	if logger == nil {
		logger = slog.Default()
	}
	logger = logger.With("where", "dns_proxy", "cid", cid)

	ln, port, err := listenRandomVsockPort(ctx)
	if err != nil {
		return nil, fmt.Errorf("listen for dns proxy: %w", err)
	}

	resolver, err := newHostDNSResolver(dnsProxyResolvConfPath, logger)
	if err != nil {
		_ = ln.Close()
		return nil, err
	}

	listener := newLimitedListener(
		&cidFilteredVsockListener{
			Listener: ln,
			cid:      cid,
			logger:   logger,
		},
		dnsProxyMaxConnections,
		logger,
	)

	proxy := &DNSProxy{
		port: port,
		srv: &dns.Server{
			Net:           "tcp",
			Listener:      listener,
			Handler:       dns.HandlerFunc(resolver.ServeDNS),
			ReadTimeout:   dnsProxyIOTimeout,
			WriteTimeout:  dnsProxyIOTimeout,
			IdleTimeout:   func() time.Duration { return dnsProxyIdleTimeout },
			MaxTCPQueries: dnsProxyMaxTCPQueries,
			MsgInvalidFunc: func(_ []byte, err error) {
				logger.Warn("dns proxy invalid message", "error", err)
			},
		},
	}

	go func() {
		<-ctx.Done()
		_ = proxy.Close()
	}()

	go func() {
		if err := proxy.srv.ActivateAndServe(); err != nil && !errors.Is(err, net.ErrClosed) {
			logger.Warn("dns proxy stopped", "error", err)
		}
	}()

	logger.Info("started dns proxy", "port", port)
	return proxy, nil
}

func (p *DNSProxy) Port() uint32 {
	if p == nil {
		return 0
	}
	return p.port
}

func (p *DNSProxy) Close() error {
	if p == nil || p.srv == nil {
		return nil
	}

	p.closeOnce.Do(func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), dnsProxyShutdownTimeout)
		defer cancel()

		p.closeErr = p.srv.ShutdownContext(shutdownCtx)
	})
	return p.closeErr
}

type limitedListener struct {
	net.Listener
	slots  chan struct{}
	logger *slog.Logger
}

func newLimitedListener(listener net.Listener, limit int, logger *slog.Logger) net.Listener {
	if limit <= 0 {
		return listener
	}
	return &limitedListener{
		Listener: listener,
		slots:    make(chan struct{}, limit),
		logger:   logger,
	}
}

func (l *limitedListener) Accept() (net.Conn, error) {
	for {
		conn, err := l.Listener.Accept()
		if err != nil {
			return nil, err
		}

		select {
		case l.slots <- struct{}{}:
			return &limitedConn{
				Conn: conn,
				release: func() {
					<-l.slots
				},
			}, nil
		default:
			l.logger.Warn("dns proxy dropped connection because workers are busy")
			_ = conn.Close()
		}
	}
}

type limitedConn struct {
	net.Conn
	once    sync.Once
	release func()
}

func (c *limitedConn) Close() error {
	err := c.Conn.Close()
	c.once.Do(c.release)
	return err
}

type hostDNSResolver struct {
	upstreams []string
	attempts  int
	timeout   time.Duration
	logger    *slog.Logger
}

func newHostDNSResolver(path string, logger *slog.Logger) (*hostDNSResolver, error) {
	config, err := dns.ClientConfigFromFile(path)
	if err != nil {
		return nil, fmt.Errorf("read host resolv.conf: %w", err)
	}
	if len(config.Servers) == 0 {
		return nil, fmt.Errorf("host resolv.conf has no nameservers")
	}

	port := config.Port
	if port == "" {
		port = "53"
	}

	upstreams := make([]string, 0, len(config.Servers))
	for _, server := range config.Servers {
		upstreams = append(upstreams, net.JoinHostPort(server, port))
	}

	timeout := time.Duration(config.Timeout) * time.Second
	if timeout <= 0 {
		timeout = dnsProxyIOTimeout
	}

	return &hostDNSResolver{
		upstreams: upstreams,
		attempts:  max(config.Attempts, 1),
		timeout:   timeout,
		logger:    logger,
	}, nil
}

func (r *hostDNSResolver) ServeDNS(w dns.ResponseWriter, req *dns.Msg) {
	resp, err := r.exchange(req)
	if err != nil {
		r.logger.Warn(
			"dns upstream exchange failed",
			"question", dnsQuestionLogValue(req),
			"error", err,
		)
		if err := w.WriteMsg(rcodeResponse(req, dns.RcodeServerFailure)); err != nil {
			r.logger.Warn("dns proxy response write failed", "error", err)
		}
		return
	}

	filterDNSResponse(resp)

	if err := w.WriteMsg(resp); err != nil {
		r.logger.Warn("dns proxy response write failed", "error", err)
	}
}

func (r *hostDNSResolver) exchange(req *dns.Msg) (*dns.Msg, error) {
	var errs []error

	for range r.attempts {
		for _, upstream := range r.upstreams {
			resp, err := exchangeDNSAt(req, upstream, r.timeout)
			if err == nil {
				return resp, nil
			}
			errs = append(errs, fmt.Errorf("%s: %w", upstream, err))
		}
	}

	return nil, errors.Join(errs...)
}

func exchangeDNSAt(req *dns.Msg, addr string, timeout time.Duration) (*dns.Msg, error) {
	resp, _, err := (&dns.Client{Net: "udp", Timeout: timeout}).Exchange(req, addr)
	if err != nil {
		return nil, err
	}
	if resp == nil {
		return nil, fmt.Errorf("empty udp response")
	}
	if !resp.Truncated {
		return resp, nil
	}

	resp, _, err = (&dns.Client{Net: "tcp", Timeout: timeout}).Exchange(req, addr)
	if err != nil {
		return nil, err
	}
	if resp == nil {
		return nil, fmt.Errorf("empty tcp response")
	}
	return resp, nil
}

func filterDNSResponse(msg *dns.Msg) {
	if msg == nil {
		return
	}
	msg.Answer = filterDNSRRs(msg.Answer)
	msg.Ns = filterDNSRRs(msg.Ns)
	msg.Extra = filterDNSRRs(msg.Extra)
}

func filterDNSRRs(rrs []dns.RR) []dns.RR {
	filtered := rrs[:0]
	for _, rr := range rrs {
		if rr := filterDNSRR(rr); rr != nil {
			filtered = append(filtered, rr)
		}
	}
	return filtered
}

func filterDNSRR(rr dns.RR) dns.RR {
	switch rr := rr.(type) {
	case *dns.A:
		if isBlockedNamespaceIP(rr.A) {
			return nil
		}
	case *dns.AAAA:
		if isBlockedNamespaceIP(rr.AAAA) {
			return nil
		}
	case *dns.SVCB:
		filterSVCBValues(&rr.Value)
	case *dns.HTTPS:
		filterSVCBValues(&rr.Value)
	}
	return rr
}

// this removes any blocked namespaces in ipv4/v6 hints
func filterSVCBValues(values *[]dns.SVCBKeyValue) {
	filtered := (*values)[:0]
	for _, value := range *values {
		switch value := value.(type) {
		case *dns.SVCBIPv4Hint:
			value.Hint = filterDNSIPs(value.Hint)
			if len(value.Hint) == 0 {
				continue
			}
		case *dns.SVCBIPv6Hint:
			value.Hint = filterDNSIPs(value.Hint)
			if len(value.Hint) == 0 {
				continue
			}
		}
		filtered = append(filtered, value)
	}
	*values = filtered
}

func filterDNSIPs(ips []net.IP) []net.IP {
	filtered := ips[:0]
	for _, ip := range ips {
		if !isBlockedNamespaceIP(ip) {
			filtered = append(filtered, ip)
		}
	}
	return filtered
}

func isBlockedNamespaceIP(ip net.IP) bool {
	if ip == nil {
		return true
	}
	if ip4 := ip.To4(); ip4 != nil {
		return isBlockedByNamespaceNets(ip4, 32)
	}
	return isBlockedByNamespaceNets(ip, 128)
}

func isBlockedByNamespaceNets(ip net.IP, bits int) bool {
	for _, blockedNet := range blockedNamespaceNets {
		if blockedNet == nil {
			continue
		}

		_, blockedBits := blockedNet.Mask.Size()
		if blockedBits != bits {
			continue
		}
		if blockedNet.Contains(ip) {
			return true
		}
	}
	return false
}

func rcodeResponse(req *dns.Msg, rcode int) *dns.Msg {
	resp := new(dns.Msg)
	if req == nil {
		resp.Rcode = rcode
		return resp
	}
	resp.SetRcode(req, rcode)
	return resp
}

func dnsQuestionLogValue(msg *dns.Msg) string {
	if msg == nil || len(msg.Question) == 0 {
		return ""
	}

	q := msg.Question[0]
	qtype := dns.TypeToString[q.Qtype]
	if qtype == "" {
		qtype = fmt.Sprintf("TYPE%d", q.Qtype)
	}
	return fmt.Sprintf("%s/%s", q.Name, qtype)
}
