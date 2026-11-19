package main

import (
	"context"
	"crypto/sha256"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"

	"go.mau.fi/mautrix-meta/pkg/messagix/socket"
)

const avatarFallbackPath = "/messaging/lightspeed/media_fallback/"

func resolveThreadAvatar(thread, picture, contact string) {
	if client == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	if source, err := url.ParseRequestURI(picture); err == nil && source.Path == avatarFallbackPath &&
		isFacebookAvatarSource(source) && source.Query().Get("entity_id") != "" {
		if resolved := resolveFallbackAvatar(ctx, source); resolved != "" {
			emit(map[string]any{"type": "avatarResolved", "thread": thread, "source": picture, "image": resolved})
			return
		}
	}
	if peer := toInt64(contact); peer > 0 {
		_, _ = runTasks(ctx, &socket.GetContactsFullTask{ContactID: peer})
	}
	raw := toInt64(strings.TrimPrefix(thread, "e:"))
	if raw <= 0 {
		return
	}
	if strings.HasPrefix(thread, "e:") {
		if hybrid := getHybridFBKey(raw); hybrid > 0 {
			raw = hybrid
		}
	}
	_, _ = runTasks(ctx, &socket.CreateThreadTask{ThreadFBID: raw, SyncGroup: int(syncGroupFor(raw)), MetadataOnly: 1})
}

func isFacebookAvatarSource(source *url.URL) bool {
	return source.User == nil && (source.Scheme == "" && source.Host == "" ||
		source.Scheme == "https" && source.Host == "www.facebook.com")
}

func resolveFallbackAvatar(ctx context.Context, source *url.URL) string {
	target := &url.URL{Scheme: "https", Host: "www.facebook.com", Path: source.Path, RawQuery: source.RawQuery}
	cache := filepath.Join(dataDir(), "media-cache")
	hash := sha256.Sum256([]byte(source.RequestURI()))
	stem := fmt.Sprintf("avatar-%x", hash[:16])
	for _, ext := range []string{"jpg", "png", "gif", "webp"} {
		path := filepath.Join(cache, stem+"."+ext)
		if info, err := os.Stat(path); err == nil && info.Size() > 0 {
			_ = os.Chtimes(path, time.Now(), time.Now())
			return path
		}
	}

	httpLayer := client.GetHTTP()
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, target.String(), nil)
	if err != nil {
		return ""
	}
	req.Header = httpLayer.BuildHeaders(true, false)
	req.Header.Set("Accept", "image/webp,image/png,image/jpeg,image/gif")
	req.Header.Set("Referer", "https://www.facebook.com/messages/")
	// The session header is only sent to the fixed first-party endpoint.
	transport := *httpLayer.HTTP
	transport.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }
	resp, err := transport.Do(req)
	if err != nil {
		return ""
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 300 && resp.StatusCode < 400 {
		next, err := resp.Location()
		if err != nil || next.Scheme != "https" || next.User != nil {
			return ""
		}
		host := next.Hostname()
		if strings.HasSuffix(host, ".fbcdn.net") || strings.HasSuffix(host, ".fbsbx.com") {
			return next.String()
		}
		return ""
	}
	if resp.StatusCode != http.StatusOK {
		return ""
	}
	const maxAvatarSize = 4 << 20
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxAvatarSize+1))
	if err != nil || len(data) == 0 || len(data) > maxAvatarSize {
		return ""
	}
	ext := map[string]string{"image/jpeg": "jpg", "image/png": "png", "image/gif": "gif", "image/webp": "webp"}[http.DetectContentType(data)]
	if ext == "" || os.MkdirAll(cache, 0o700) != nil {
		return ""
	}
	file, err := os.CreateTemp(cache, "avatar-part-*")
	if err != nil {
		return ""
	}
	defer os.Remove(file.Name())
	_, err = file.Write(data)
	closeErr := file.Close()
	path := filepath.Join(cache, stem+"."+ext)
	if err != nil || closeErr != nil || os.Rename(file.Name(), path) != nil {
		return ""
	}
	return path
}
