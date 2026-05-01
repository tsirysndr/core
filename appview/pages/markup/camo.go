package markup

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
)

func GenerateCamoURL(baseURL, secret, imageURL string) string {
	h := hmac.New(sha256.New, []byte(secret))
	h.Write([]byte(imageURL))
	signature := hex.EncodeToString(h.Sum(nil))
	hexURL := hex.EncodeToString([]byte(imageURL))
	return fmt.Sprintf("%s/%s/%s", baseURL, signature, hexURL)
}

func (rctx *RenderContext) camoImageLinkTransformer(dst string) string {
	if rctx.CamoUrl != "" && rctx.CamoSecret != "" {
		return GenerateCamoURL(rctx.CamoUrl, rctx.CamoSecret, dst)
	}

	return dst
}
