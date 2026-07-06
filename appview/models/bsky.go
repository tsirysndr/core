package models

import (
	"encoding/json"
	"time"
)

type BskyPost struct {
	AuthorDid   string
	Rkey        string
	Text        string
	CreatedAt   time.Time
	Langs       []string
	Tags        []string
	Embed       *PostEmbed
	Facets      json.RawMessage
	LikeCount   int64
	ReplyCount  int64
	RepostCount int64
	QuoteCount  int64
}

type PostEmbed struct {
	Images   []PostImage   `json:"images,omitempty"`
	External *PostExternal `json:"external,omitempty"`
	Video    *PostVideo    `json:"video,omitempty"`
}

type AspectRatio struct {
	Width  int64 `json:"width"`
	Height int64 `json:"height"`
}

type PostImage struct {
	Fullsize    string       `json:"fullsize"`
	Thumb       string       `json:"thumb"`
	Alt         string       `json:"alt"`
	AspectRatio *AspectRatio `json:"aspectRatio,omitempty"`
}

type PostExternal struct {
	Uri         string `json:"uri"`
	Title       string `json:"title"`
	Description string `json:"description"`
	Thumb       string `json:"thumb"`
}

type PostVideo struct {
	Playlist  string `json:"playlist"`
	Thumbnail string `json:"thumbnail"`
	Alt       string `json:"alt"`
}
