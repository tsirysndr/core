// refuses outbound dials to special-purpose addresses, for anywhere
// spindle fetches user-influenced urls (workflow caches, PDS blob
// fetches)
package netguard

import (
	"fmt"
	"net"
	"syscall"
)

// https://www.iana.org/assignments/iana-ipv4-special-registry/iana-ipv4-special-registry.xhtml
// https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry.xhtml
// https://datatracker.ietf.org/doc/rfc6890/
var BlockedRoutes = []string{
	"0.0.0.0/8",       // unspecified / "this network" addresses
	"10.0.0.0/8",      // private network
	"100.64.0.0/10",   // shared carrier-grade nat space
	"127.0.0.0/8",     // loopback
	"169.254.0.0/16",  // link-local / autoconfiguration
	"172.16.0.0/12",   // private network
	"192.0.0.0/24",    // ietf protocol assignments
	"192.0.2.0/24",    // documentation / examples
	"192.88.99.0/24",  // deprecated 6to4 relay anycast
	"192.168.0.0/16",  // private network
	"198.18.0.0/15",   // benchmarking / testing
	"198.51.100.0/24", // documentation / examples
	"203.0.113.0/24",  // documentation / examples
	"224.0.0.0/4",     // multicast
	"240.0.0.0/4",     // reserved / future use, includes limited broadcast
	"::/128",          // unspecified address
	"::1/128",         // loopback
	"::ffff:0:0/96",   // ipv4-mapped addresses
	"64:ff9b::/96",    // ipv4/ipv6 translation prefix
	"100::/64",        // discard-only prefix
	"2001::/23",       // ietf protocol assignments
	"2001:db8::/32",   // documentation / examples
	"2002::/16",       // deprecated 6to4 addressing
	"fc00::/7",        // unique local addresses
	"fe80::/10",       // link-local unicast
	"ff00::/8",        // multicast
}

var BlockedNets = func() []*net.IPNet {
	nets := make([]*net.IPNet, 0, len(BlockedRoutes))
	for _, route := range BlockedRoutes {
		_, ipnet, err := net.ParseCIDR(route)
		if err != nil {
			panic(fmt.Sprintf("parse blocked route %q: %v", route, err))
		}
		nets = append(nets, ipnet)
	}
	return nets
}()

// net.Dialer Control func rejecting blocked special-purpose addresses.
// this should run after dns resolution, so it should cover any rebinding tricks
func RefuseSpecialPurposeAddrs(network, address string, _ syscall.RawConn) error {
	host, _, err := net.SplitHostPort(address)
	if err != nil {
		return fmt.Errorf("split dial address %q: %w", address, err)
	}
	ip := net.ParseIP(host)
	if ip == nil {
		return fmt.Errorf("refusing to dial non-IP address %q", host)
	}
	bits := 128
	if ip4 := ip.To4(); ip4 != nil {
		ip = ip4
		bits = 32
	}
	for _, ipnet := range BlockedNets {
		_, blockedBits := ipnet.Mask.Size()
		if blockedBits != bits {
			continue
		}
		if ipnet.Contains(ip) {
			return fmt.Errorf("refusing to dial %s: %s is blocked", ip, ipnet)
		}
	}
	return nil
}
