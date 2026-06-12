package microvm

import (
	"fmt"
	"slices"
)

type manifestWorkflow struct {
	Image          string            `yaml:"image"`
	Services       map[string]any    `yaml:"services"`
	Virtualisation map[string]any    `yaml:"virtualisation"`
	Dependencies   []string          `yaml:"dependencies"`
	Registry       map[string]any    `yaml:"registry"`
	Environment    map[string]string `yaml:"environment"`
	Caches         map[string]string `yaml:"caches"`
	Steps          []struct {
		Name        string            `yaml:"name"`
		Command     string            `yaml:"command"`
		Environment map[string]string `yaml:"environment"`
	} `yaml:"steps"`
}

// flattens the caches map into sorted substituter URLs and trusted public keys
func workflowCaches(caches map[string]string) (urls []string, keys []string, err error) {
	for cacheURL, key := range caches {
		urls = append(urls, cacheURL)
		if key != "" {
			keys = append(keys, key)
		}
	}
	if _, err := parseCacheUpstreams(urls); err != nil {
		return nil, nil, fmt.Errorf("caches: %w", err)
	}
	slices.Sort(urls)
	slices.Sort(keys)
	return urls, keys, nil
}

type manifestConfig struct {
	Services       map[string]any `yaml:"services"     json:"services,omitempty"`
	Virtualisation map[string]any `yaml:"virtualisation" json:"virtualisation,omitempty"`
	Dependencies   []string       `yaml:"dependencies" json:"dependencies,omitempty"`
	Registry       map[string]any `yaml:"registry"     json:"registry,omitempty"`
}

func (c manifestConfig) Enabled() bool {
	return len(c.Services) > 0 || len(c.Virtualisation) > 0 || len(c.Dependencies) > 0 || len(c.Registry) > 0
}
