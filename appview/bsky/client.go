package bsky

import (
	"context"
	"encoding/json"
	"log"
	"net/http"
	"time"

	"github.com/bluesky-social/indigo/atproto/syntax"
	"github.com/bluesky-social/indigo/xrpc"
	"tangled.org/core/appview/models"
	"tangled.org/core/consts"
)

type rawFeedOutput struct {
	Cursor *string       `json:"cursor,omitempty"`
	Feed   []rawFeedItem `json:"feed"`
}

type rawFeedItem struct {
	Post rawPostView `json:"post"`
}

type rawPostView struct {
	URI    string `json:"uri"`
	Author struct {
		DID string `json:"did"`
	} `json:"author"`
	Record      rawRecord       `json:"record"`
	Embed       json.RawMessage `json:"embed,omitempty"`
	LikeCount   *int64          `json:"likeCount,omitempty"`
	ReplyCount  *int64          `json:"replyCount,omitempty"`
	RepostCount *int64          `json:"repostCount,omitempty"`
	QuoteCount  *int64          `json:"quoteCount,omitempty"`
}

type rawRecord struct {
	Text      string          `json:"text"`
	CreatedAt string          `json:"createdAt"`
	Langs     []string        `json:"langs,omitempty"`
	Tags      []string        `json:"tags,omitempty"`
	Facets    json.RawMessage `json:"facets,omitempty"`
}

type rawEmbedType struct {
	Type string `json:"$type"`
}

func FetchPosts(ctx context.Context, c *xrpc.Client, limit int, cursor string) ([]models.BskyPost, string, error) {
	var out rawFeedOutput

	params := map[string]any{
		"actor":  consts.TangledDid,
		"filter": "posts_no_replies",
		"limit":  int64(limit),
	}
	if cursor != "" {
		params["cursor"] = cursor
	}
	if err := c.Do(ctx, http.MethodGet, "", "app.bsky.feed.getAuthorFeed", params, nil, &out); err != nil {
		return nil, "", err
	}

	var posts []models.BskyPost
	for _, feedItem := range out.Feed {
		raw := feedItem.Post

		if len(raw.Embed) > 0 {
			var t rawEmbedType
			json.Unmarshal(raw.Embed, &t)
			if t.Type == "app.bsky.embed.record#view" {
				continue
			}
		}

		atUri, err := syntax.ParseATURI(raw.URI)
		if err != nil {
			log.Println("bsky: parse AT URI:", err)
			continue
		}

		createdAt, err := time.Parse(time.RFC3339, raw.Record.CreatedAt)
		if err != nil {
			log.Println("bsky: parse createdAt:", err)
			continue
		}

		post := models.BskyPost{
			AuthorDid: raw.Author.DID,
			Rkey:      atUri.RecordKey().String(),
			Text:      raw.Record.Text,
			CreatedAt: createdAt,
			Langs:     raw.Record.Langs,
			Tags:      raw.Record.Tags,
			Facets:    raw.Record.Facets,
		}

		if raw.LikeCount != nil {
			post.LikeCount = *raw.LikeCount
		}
		if raw.ReplyCount != nil {
			post.ReplyCount = *raw.ReplyCount
		}
		if raw.RepostCount != nil {
			post.RepostCount = *raw.RepostCount
		}
		if raw.QuoteCount != nil {
			post.QuoteCount = *raw.QuoteCount
		}

		if len(raw.Embed) > 0 {
			embed, err := parseEmbed(raw.Embed)
			if err != nil {
				log.Println("bsky: parse embed:", err)
			}
			post.Embed = embed
		}

		posts = append(posts, post)
	}

	nextCursor := ""
	if out.Cursor != nil {
		nextCursor = *out.Cursor
	}

	return posts, nextCursor, nil
}

func parseEmbed(raw json.RawMessage) (*models.PostEmbed, error) {
	var t rawEmbedType
	if err := json.Unmarshal(raw, &t); err != nil {
		return nil, err
	}

	switch t.Type {
	case "app.bsky.embed.images#view":
		var v struct {
			Images []models.PostImage `json:"images"`
		}
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, err
		}
		return &models.PostEmbed{Images: v.Images}, nil

	case "app.bsky.embed.gallery#view":
		// gallery items use "thumbnail" instead of "thumb"
		var v struct {
			Items []struct {
				Fullsize    string              `json:"fullsize"`
				Thumbnail   string              `json:"thumbnail"`
				Alt         string              `json:"alt"`
				AspectRatio *models.AspectRatio `json:"aspectRatio,omitempty"`
			} `json:"items"`
		}
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, err
		}
		embed := &models.PostEmbed{}
		for _, item := range v.Items {
			embed.Images = append(embed.Images, models.PostImage{
				Fullsize:    item.Fullsize,
				Thumb:       item.Thumbnail,
				Alt:         item.Alt,
				AspectRatio: item.AspectRatio,
			})
		}
		return embed, nil

	case "app.bsky.embed.external#view":
		var v struct {
			External models.PostExternal `json:"external"`
		}
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, err
		}
		return &models.PostEmbed{External: &v.External}, nil

	case "app.bsky.embed.video#view":
		var v models.PostVideo
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, err
		}
		return &models.PostEmbed{Video: &v}, nil

	case "app.bsky.embed.recordWithMedia#view":
		var v struct {
			Media json.RawMessage `json:"media"`
		}
		if err := json.Unmarshal(raw, &v); err != nil {
			return nil, err
		}
		return parseEmbed(v.Media)

	default:
		return nil, nil
	}
}
