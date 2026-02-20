package cloudflare

import (
	"context"
	"fmt"

	cf "github.com/cloudflare/cloudflare-go/v6"
	"github.com/cloudflare/cloudflare-go/v6/dns"
)

type DNSRecord struct {
	Type    string
	Name    string
	Content string
	TTL     int
	Proxied bool
}

func (cl *Client) CreateDNSRecord(ctx context.Context, record DNSRecord) (string, error) {
	var body dns.RecordNewParamsBodyUnion

	switch record.Type {
	case "A":
		body = dns.ARecordParam{
			Name:    cf.F(record.Name),
			TTL:     cf.F(dns.TTL(record.TTL)),
			Type:    cf.F(dns.ARecordTypeA),
			Content: cf.F(record.Content),
			Proxied: cf.F(record.Proxied),
		}
	case "CNAME":
		body = dns.CNAMERecordParam{
			Name:    cf.F(record.Name),
			TTL:     cf.F(dns.TTL(record.TTL)),
			Type:    cf.F(dns.CNAMERecordTypeCNAME),
			Content: cf.F(record.Content),
			Proxied: cf.F(record.Proxied),
		}
	default:
		return "", fmt.Errorf("unsupported DNS record type: %s", record.Type)
	}

	result, err := cl.api.DNS.Records.New(ctx, dns.RecordNewParams{
		ZoneID: cf.F(cl.zone),
		Body:   body,
	})
	if err != nil {
		return "", fmt.Errorf("failed to create DNS record: %w", err)
	}
	return result.ID, nil
}

func (cl *Client) DeleteDNSRecord(ctx context.Context, recordID string) error {
	_, err := cl.api.DNS.Records.Delete(ctx, recordID, dns.RecordDeleteParams{
		ZoneID: cf.F(cl.zone),
	})
	if err != nil {
		return fmt.Errorf("failed to delete DNS record: %w", err)
	}
	return nil
}
