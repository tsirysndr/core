package microvm

import (
	"net"
	"testing"

	"github.com/miekg/dns"
)

func TestFilterDNSResponseDropsBlockedAddressRecords(t *testing.T) {
	msg := new(dns.Msg)
	msg.Answer = []dns.RR{
		&dns.CNAME{Hdr: dns.RR_Header{Name: "cache.example.", Rrtype: dns.TypeCNAME, Class: dns.ClassINET}, Target: "edge.example."},
		&dns.A{Hdr: dns.RR_Header{Name: "edge.example.", Rrtype: dns.TypeA, Class: dns.ClassINET}, A: net.ParseIP("1.1.1.1")},
		&dns.A{Hdr: dns.RR_Header{Name: "edge.example.", Rrtype: dns.TypeA, Class: dns.ClassINET}, A: net.ParseIP("10.0.0.1")},
		&dns.AAAA{Hdr: dns.RR_Header{Name: "edge.example.", Rrtype: dns.TypeAAAA, Class: dns.ClassINET}, AAAA: net.ParseIP("2606:4700:4700::1111")},
		&dns.AAAA{Hdr: dns.RR_Header{Name: "edge.example.", Rrtype: dns.TypeAAAA, Class: dns.ClassINET}, AAAA: net.ParseIP("fd7a:115c:a1e0::53")},
	}
	msg.Extra = []dns.RR{
		&dns.A{Hdr: dns.RR_Header{Name: "private.example.", Rrtype: dns.TypeA, Class: dns.ClassINET}, A: net.ParseIP("192.168.1.2")},
		&dns.A{Hdr: dns.RR_Header{Name: "public.example.", Rrtype: dns.TypeA, Class: dns.ClassINET}, A: net.ParseIP("8.8.8.8")},
	}

	filterDNSResponse(msg)

	if len(msg.Answer) != 3 {
		t.Fatalf("filtered answer len = %d, want 3: %#v", len(msg.Answer), msg.Answer)
	}
	if _, ok := msg.Answer[0].(*dns.CNAME); !ok {
		t.Fatalf("answer[0] = %T, want CNAME", msg.Answer[0])
	}
	if a, ok := msg.Answer[1].(*dns.A); !ok || !a.A.Equal(net.ParseIP("1.1.1.1")) {
		t.Fatalf("answer[1] = %#v, want public A", msg.Answer[1])
	}
	if aaaa, ok := msg.Answer[2].(*dns.AAAA); !ok || !aaaa.AAAA.Equal(net.ParseIP("2606:4700:4700::1111")) {
		t.Fatalf("answer[2] = %#v, want public AAAA", msg.Answer[2])
	}
	if len(msg.Extra) != 1 {
		t.Fatalf("filtered extra len = %d, want 1: %#v", len(msg.Extra), msg.Extra)
	}
}

func TestFilterDNSResponseFiltersSVCBAddressHints(t *testing.T) {
	msg := new(dns.Msg)
	msg.Answer = []dns.RR{
		&dns.HTTPS{
			SVCB: dns.SVCB{
				Hdr:      dns.RR_Header{Name: "svc.example.", Rrtype: dns.TypeHTTPS, Class: dns.ClassINET},
				Priority: 1,
				Target:   ".",
				Value: []dns.SVCBKeyValue{
					&dns.SVCBIPv4Hint{Hint: []net.IP{net.ParseIP("10.0.0.1"), net.ParseIP("8.8.8.8")}},
					&dns.SVCBIPv6Hint{Hint: []net.IP{net.ParseIP("fd7a:115c:a1e0::53"), net.ParseIP("2001:4860:4860::8888")}},
				},
			},
		},
	}

	filterDNSResponse(msg)

	https := msg.Answer[0].(*dns.HTTPS)
	if len(https.Value) != 2 {
		t.Fatalf("https values len = %d, want 2", len(https.Value))
	}
	ipv4 := https.Value[0].(*dns.SVCBIPv4Hint)
	if len(ipv4.Hint) != 1 || !ipv4.Hint[0].Equal(net.ParseIP("8.8.8.8")) {
		t.Fatalf("ipv4 hints = %v, want [8.8.8.8]", ipv4.Hint)
	}
	ipv6 := https.Value[1].(*dns.SVCBIPv6Hint)
	if len(ipv6.Hint) != 1 || !ipv6.Hint[0].Equal(net.ParseIP("2001:4860:4860::8888")) {
		t.Fatalf("ipv6 hints = %v, want [2001:4860:4860::8888]", ipv6.Hint)
	}
}
