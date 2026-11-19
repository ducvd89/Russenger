// relay-helper — Relay's backend daemon.
//
// Embeds messagix (Meta's reverse-engineered protocol) and exposes a tiny,
// stable JSON-over-stdio API for the SwiftUI app:
//
//	stdin  (commands, one JSON object per line):
//	  {"cmd":"login","cookies":"datr=…; c_user=…; xs=…"}
//	  {"cmd":"send","thread":123,"text":"hello"}
//
//	stdout (events, one JSON object per line):
//	  {"type":"ready","self":123}
//	  {"type":"contact","id":1,"name":"…","firstName":"…","avatar":"…"}
//	  {"type":"thread","id":1,"name":"…","snippet":"…","lastActivity":…,"unread":true}
//	  {"type":"message","id":"…","thread":1,"sender":1,"text":"…","ts":…,"live":true}
//	  {"type":"error","msg":"…"}
//
// All logging goes to STDERR so stdout stays a clean protocol stream.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"image"
	_ "image/gif"
	_ "image/jpeg"
	_ "image/png"
	"net/http"
	neturl "net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/rs/zerolog"
	"go.mau.fi/mautrix-meta/pkg/messagix"
	"go.mau.fi/mautrix-meta/pkg/messagix/cookies"
	"go.mau.fi/mautrix-meta/pkg/messagix/httpclient"
	"go.mau.fi/mautrix-meta/pkg/messagix/methods"
	"go.mau.fi/mautrix-meta/pkg/messagix/socket"
	"go.mau.fi/mautrix-meta/pkg/messagix/table"
	"go.mau.fi/mautrix-meta/pkg/messagix/types"
)

var (
	log    zerolog.Logger
	outMu  sync.Mutex
	client *messagix.Client
)

func emit(obj map[string]any) {
	outMu.Lock()
	defer outMu.Unlock()
	b, err := json.Marshal(obj)
	if err != nil {
		return
	}
	os.Stdout.Write(b)
	os.Stdout.Write([]byte("\n"))
}

func dataDir() string {
	if dir := os.Getenv("RELAY_DATA_DIR"); dir != "" {
		return dir
	}
	dir, _ := os.UserConfigDir()
	return filepath.Join(dir, "Relay")
}

func main() {
	// the libraries log protocol rows (with message data) at warning level: keep errors only
	zerolog.SetGlobalLevel(zerolog.ErrorLevel)
	log = zerolog.New(zerolog.ConsoleWriter{Out: os.Stderr, TimeFormat: "15:04:05"}).Level(zerolog.WarnLevel).With().Timestamp().Logger()

	// Read commands from stdin in the background.
	cmds := make(chan map[string]any, 16)
	go func() {
		sc := bufio.NewScanner(os.Stdin)
		sc.Buffer(make([]byte, 1024*1024), 8*1024*1024)
		for sc.Scan() {
			line := strings.TrimSpace(sc.Text())
			if line == "" {
				continue
			}
			var m map[string]any
			if err := json.Unmarshal([]byte(line), &m); err != nil {
				log.Warn().Err(err).Msg("bad command")
				continue
			}
			cmds <- m
		}
		os.Exit(0) // stdin closed → app quit
	}()

	// First command must be login (or a --cookies file arg for CLI testing).
	var cookieStr string
	if len(os.Args) > 1 {
		if raw, err := os.ReadFile(os.Args[1]); err == nil {
			cookieStr = strings.TrimSpace(string(raw))
		}
	}
	for cookieStr == "" {
		m := <-cmds
		if m["cmd"] == "login" {
			cookieStr, _ = m["cookies"].(string)
		}
	}

	if err := connect(cookieStr); err != nil {
		emit(map[string]any{"type": "error", "msg": err.Error()})
		log.Fatal().Err(err).Msg("connect failed")
	}

	// Sends are queued so two quick messages cannot overtake each other.
	sendQ := make(chan func(), 256)
	go func() {
		for job := range sendQ {
			job()
		}
	}()

	// Serve commands.
	for m := range cmds {
		switch m["cmd"] {
		case "send":
			thread, _ := m["thread"].(string)
			text, _ := m["text"].(string)
			replyID, _ := m["replyId"].(string)
			replySender, _ := m["replySender"].(string)
			clientTag, _ := m["clientTag"].(string)
			mentions := parseMentionSpans(m["mentions"])
			if thread != "" && text != "" {
				sendQ <- func() { sendMessage(thread, text, replyID, replySender, clientTag, mentions) }
			}
		case "fetchContact":
			// App asks us to resolve a person it knows by FBID but has no name/avatar for.
			if id := toInt64(m["id"]); id != 0 {
				go fetchContact(id)
			}
		case "sendMedia":
			thread, _ := m["thread"].(string)
			path, _ := m["path"].(string)
			caption, _ := m["caption"].(string)
			if thread != "" && path != "" {
				sendQ <- func() { sendMediaMessage(thread, path, caption) }
			}
		case "resolveThreadAvatar":
			thread, _ := m["thread"].(string)
			picture, _ := m["picture"].(string)
			contact, _ := m["contact"].(string)
			go resolveThreadAvatar(thread, picture, contact)
		case "loadMoreThreads":
			// the app knows the oldest chat it has stored; start the next page after it
			if k := toInt64(m["refKey"]); k != 0 {
				oldestMu.Lock()
				ts := int64(toInt64(m["refTs"]))
				if oldestTs == 0 || ts < oldestTs {
					oldestKey, oldestTs = k, ts
				}
				oldestMu.Unlock()
			}
			go loadMoreThreads(40)
		case "sendSticker":
			thread, _ := m["thread"].(string)
			if id := toInt64(m["stickerId"]); thread != "" && id != 0 {
				picture, _ := m["picture"].(string)
				sendQ <- func() { sendSticker(thread, id, picture) }
			}
		case "sendGif":
			thread, _ := m["thread"].(string)
			url, _ := m["url"].(string)
			if thread != "" && url != "" {
				go sendGif(thread, url)
			}
		case "refreshThread":
			// App opened a thread — pull anything newer than what it has.
			if thread, _ := m["thread"].(string); thread != "" {
				sinceTs := toInt64(m["sinceTs"])
				go refreshThread(thread, sinceTs)
			}
		case "syncOpenHistory":
			if thread, _ := m["thread"].(string); thread != "" {
				beforeID, _ := m["beforeId"].(string)
				older, _ := m["older"].(bool)
				go syncOpenHistory(thread, beforeID, toInt64(m["beforeTs"]), older)
			}
		case "fetchQuote":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			go fetchQuotedMessage(thread, id, toInt64(m["ts"]))
		case "react":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			emoji, _ := m["emoji"].(string)
			fromMe, _ := m["fromMe"].(bool)
			if thread != "" && id != "" {
				go sendReaction(thread, id, emoji, fromMe)
			}
		case "unsend":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			if thread != "" && id != "" {
				go unsendMessage(thread, id)
			}
		case "edit":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			text, _ := m["text"].(string)
			if thread != "" && id != "" && text != "" {
				go editMessage(thread, id, text)
			}
		case "markReadServer":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			sender, _ := m["sender"].(string)
			ts := toInt64(m["ts"])
			var ids []string
			if arr, ok := m["ids"].([]any); ok {
				for _, v := range arr {
					if s, ok := v.(string); ok && s != "" {
						ids = append(ids, s)
					}
				}
			}
			if thread != "" {
				go markReadServer(thread, id, ids, sender, ts)
			}
		case "mute":
			thread, _ := m["thread"].(string)
			muted, _ := m["muted"].(bool)
			if thread != "" {
				go setMute(thread, muted)
			}
		case "deleteThread":
			if thread, _ := m["thread"].(string); thread != "" {
				go deleteThread(thread)
			}
		case "renameThread":
			thread, _ := m["thread"].(string)
			name, _ := m["name"].(string)
			if thread != "" {
				go renameThread(thread, name)
			}
		case "addMembers":
			if thread, _ := m["thread"].(string); thread != "" {
				go addMembers(thread, m["ids"])
			}
		case "removeMember":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			if thread != "" && id != "" {
				go removeMember(thread, id)
			}
		case "setAdmin":
			thread, _ := m["thread"].(string)
			id, _ := m["id"].(string)
			admin, _ := m["admin"].(bool)
			if thread != "" && id != "" {
				go setThreadAdmin(thread, id, admin)
			}
		case "leaveThread":
			if thread, _ := m["thread"].(string); thread != "" {
				go leaveThread(thread)
			}
		case "setGroupPhoto":
			thread, _ := m["thread"].(string)
			path, _ := m["path"].(string)
			if thread != "" && path != "" {
				go setGroupPhoto(thread, path)
			}
		case "createGroup":
			name, _ := m["name"].(string)
			go createGroup(name, m["ids"])
		case "subscribePresence":
			// App asks us to watch a person's online / last-seen status (E2EE contacts).
			if id, _ := m["id"].(string); id != "" {
				go subscribePresence(id)
			}
		case "historyStatus":
			go historyStatus()
		case "restoreHistory":
			pin, _ := m["pin"].(string)
			go restoreHistory(pin)
		case "loadEncryptedHistory":
			thread, _ := m["thread"].(string)
			if thread != "" {
				window, _ := m["window"].(bool)
				go loadEncryptedHistory(thread, toInt64(m["before"]), window)
			}
		case "loadEarlier":
			// App asks for older messages before the oldest one it has in a thread.
			thread, _ := m["thread"].(string)
			oldestID, _ := m["oldestId"].(string)
			oldestTs := toInt64(m["oldestTs"])
			fromMe, _ := m["fromMe"].(bool)
			if thread != "" {
				go loadEarlier(thread, oldestID, oldestTs, fromMe)
			}
		}
	}
}

func connect(cookieStr string) error {
	// RELAY_PLATFORM=facebook targets facebook.com (messenger.com was retired in 2026).
	platform := types.Messenger
	if strings.EqualFold(os.Getenv("RELAY_PLATFORM"), "facebook") {
		platform = types.Facebook
	}
	c := &cookies.Cookies{Platform: platform}
	vals := map[cookies.MetaCookieName]string{}
	for _, part := range strings.Split(cookieStr, ";") {
		kv := strings.SplitN(strings.TrimSpace(part), "=", 2)
		if len(kv) == 2 {
			vals[cookies.MetaCookieName(kv[0])] = kv[1]
		}
	}
	c.UpdateValues(vals)
	log.Info().Int64("user", c.GetUserID()).Bool("loggedin", c.IsLoggedIn()).Msg("cookies")

	// No valid session → tell the app to show the in-app login.
	if !c.IsLoggedIn() {
		emit(map[string]any{"type": "needLogin"})
		return errors.New("not logged in: no valid session cookies")
	}

	client = messagix.NewClient(c, log.Level(zerolog.ErrorLevel), &messagix.Config{})
	client.SetEventHandler(func(ctx context.Context, evt any) {
		switch e := evt.(type) {
		case *messagix.ConnectedEvent, *messagix.ReconnectedEvent:
			emit(map[string]any{"type": "ready", "self": fmtID(c.GetUserID())})
		case *messagix.TransientDisconnectEvent:
			log.Warn().Err(e.Err).Msg("meta socket disconnected")
		case *messagix.PermanentErrorEvent:
			emit(map[string]any{"type": "error", "msg": "connection: " + e.Err.Error()})
			os.Exit(1)
		case *table.LSTable:
			if n := len(e.LSDeleteThenInsertThread) + len(e.LSUpdateOrInsertThread); n > 0 {
				threadRowsSeen.Add(int64(n))
			}
			processTable(e, !threadBackfill.Load())
			client.PostHandlePublishResponse(e)
		}
	})

	ctx := context.Background()
	user, tbl, err := client.LoadMessagesPage(ctx)
	if err != nil {
		return err
	}
	selfFBID = fmtID(c.GetUserID())
	emit(map[string]any{"type": "self", "id": selfFBID, "name": user.GetName()})
	if tbl != nil {
		processTable(tbl, false)
	}
	if err := client.Connect(ctx); err != nil {
		return err
	}
	emit(map[string]any{"type": "ready", "self": selfFBID})
	go watchRecentInbox()
	// the first sync only carries the newest chats; the app asks for older pages once it has
	// told us where its stored list ends (command loadMoreThreads)

	// Connect the encrypted (E2EE / WhatsApp-protocol) channel for personal chats.
	go func() {
		if err := setupE2EE(context.Background(), c.GetUserID()); err != nil {
			log.Warn().Err(err).Msg("E2EE setup failed")
			emit(map[string]any{"type": "error", "msg": "E2EE: " + err.Error()})
		}
	}()
	return nil
}

// Thread metadata & E2EE / Hybrid mapping state.
// Meta serves E2EE chats in both WhatsApp/Armadillo ("e:<jid>") and Lightspeed metadata
// tables (SyncGroup 95 / 1, sometimes under a separate hybrid fbKey mapped to ThreadJID).
// Tracking these mappings ensures threads and messages never split across two IDs and read
// watermarks are updated on both transports.
type watermark struct {
	readUpTo     int64
	lastActivity int64
}

var (
	metaMu            sync.Mutex
	e2eeKeys          = map[int64]bool{}
	e2eeGroups        = map[string]bool{}
	hybridFBToJID     = map[int64]int64{}
	hybridJIDToFB     = map[int64]int64{}
	cutoverOpenToE2EE = map[int64]int64{}
	cutoverE2EEToOpen = map[int64]int64{}
	threadSyncGroup   = map[int64]int64{}
	threadTypes       = map[int64]table.ThreadType{}
	threadWatermarks  = map[string]watermark{}
	attachLabels      = map[string]string{}

	rangeMu      sync.Mutex
	rangeChanged = make(chan struct{})
	lastMsgRange = map[int64]*table.LSInsertNewMessageRange{}
	rangeGen     = map[int64]uint64{}
)

func markE2EEKey(tid int64) {
	if tid != 0 {
		metaMu.Lock()
		e2eeKeys[tid] = true
		metaMu.Unlock()
	}
}

func isE2EEKey(tid int64) bool {
	metaMu.Lock()
	defer metaMu.Unlock()
	return e2eeKeys[tid] || threadTypes[tid].IsWhatsApp()
}

func markE2EEGroup(user string) {
	if user != "" {
		metaMu.Lock()
		e2eeGroups[user] = true
		metaMu.Unlock()
	}
}

func isE2EEGroup(user string) bool {
	metaMu.Lock()
	defer metaMu.Unlock()
	return e2eeGroups[user]
}

func resolveThreadLocked(raw int64) (emitID string, jid int64, encrypted bool) {
	jid = raw
	if m := hybridFBToJID[raw]; m != 0 {
		jid = m
	} else if m := cutoverOpenToE2EE[raw]; m != 0 {
		jid = m
	}
	encrypted = e2eeKeys[jid] || e2eeKeys[raw] || threadTypes[jid].IsWhatsApp() || threadTypes[raw].IsWhatsApp()
	if encrypted {
		return "e:" + fmtID(jid), jid, true
	}
	return fmtID(jid), jid, false
}

func emitThreadID(tid int64) string {
	metaMu.Lock()
	defer metaMu.Unlock()
	id, _, _ := resolveThreadLocked(tid)
	return id
}

func noteThreadWatermark(threadID string, readUpTo, lastActivity int64) {
	if threadID == "" {
		return
	}
	metaMu.Lock()
	wm := threadWatermarks[threadID]
	if readUpTo > wm.readUpTo {
		wm.readUpTo = readUpTo
	}
	if lastActivity > wm.lastActivity {
		wm.lastActivity = lastActivity
	}
	threadWatermarks[threadID] = wm
	metaMu.Unlock()
}

func getThreadWatermark(threadID string) watermark {
	metaMu.Lock()
	defer metaMu.Unlock()
	return threadWatermarks[threadID]
}

func syncGroupFor(tid int64) int64 {
	metaMu.Lock()
	defer metaMu.Unlock()
	if tt := threadTypes[tid]; tt >= 18 && tt <= 26 {
		return 104
	}
	if sg := threadSyncGroup[tid]; sg > 0 {
		return sg
	}
	return 1
}

func cursorDBFor(sg int64) int64 {
	switch sg {
	case 95, 104:
		return sg
	default:
		return 1
	}
}

func getHybridFBKey(jid int64) int64 {
	metaMu.Lock()
	defer metaMu.Unlock()
	return hybridJIDToFB[jid]
}

func getOpenThreadForE2EE(jid int64) int64 {
	metaMu.Lock()
	defer metaMu.Unlock()
	return cutoverE2EEToOpen[jid]
}

func setAttachLabel(msgID, label string) {
	if msgID == "" || label == "" {
		return
	}
	metaMu.Lock()
	if len(attachLabels) > 10000 {
		attachLabels = map[string]string{}
	}
	attachLabels[msgID] = label
	metaMu.Unlock()
}

func getAttachLabel(msgID string) string {
	metaMu.Lock()
	defer metaMu.Unlock()
	return attachLabels[msgID]
}

func noteMessageRange(rng *table.LSInsertNewMessageRange) {
	if rng == nil || rng.ThreadKey == 0 {
		return
	}
	rangeMu.Lock()
	lastMsgRange[rng.ThreadKey] = rng
	rangeGen[rng.ThreadKey]++
	close(rangeChanged)
	rangeChanged = make(chan struct{})
	rangeMu.Unlock()
}

func getLastRange(tid int64) (*table.LSInsertNewMessageRange, uint64) {
	rangeMu.Lock()
	defer rangeMu.Unlock()
	return lastMsgRange[tid], rangeGen[tid]
}

func waitForRangeUpdate(tid int64, prevGen uint64, timeout time.Duration) {
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()
	for {
		rangeMu.Lock()
		gen, ch := rangeGen[tid], rangeChanged
		rangeMu.Unlock()
		if gen > prevGen {
			return
		}
		select {
		case <-ch:
		case <-deadline.C:
			return
		}
	}
}

func runTasks(ctx context.Context, tasks ...socket.Task) (*table.LSTable, error) {
	if client == nil {
		return nil, fmt.Errorf("not connected")
	}
	_ = client.WaitUntilCanSendMessages(ctx, 15*time.Second)
	tbl, err := client.ExecuteTasks(ctx, tasks...)
	if err == nil && tbl != nil {
		processTable(tbl, false)
		client.PostHandlePublishResponse(tbl)
	}
	return tbl, err
}

// normalizeFolder maps Meta's many folder names (incl. the "e2ee_cutover_*"
// variants for encrypted threads) onto the four buckets the inbox shows.
func normalizeFolder(name string) string {
	switch {
	case strings.Contains(name, "pending"), strings.Contains(name, "other"):
		return "requests"
	case strings.Contains(name, "spam"):
		return "spam"
	case strings.Contains(name, "archived"):
		return "archived"
	case name == "montage" || name == "hidden" || name == "disabled" || name == "blocked":
		return "hidden"
	default:
		return "inbox"
	}
}

func attachTypeLabel(t table.AttachmentType) string {
	switch t {
	case table.AttachmentTypeImage, table.AttachmentTypeAnimatedImage:
		return "📷 Photo"
	case table.AttachmentTypeVideo:
		return "🎬 Video"
	case table.AttachmentTypeAudio:
		return "🎤 Voice message"
	case table.AttachmentTypeSticker:
		return "💟 Sticker"
	case table.AttachmentTypeFile:
		return "📎 File"
	default:
		return "📎 Attachment"
	}
}

// ctaURL caches CTA urls by attachment fbid: the CTA row can arrive before or after its XMA row.
var ctaURL sync.Map

// unwrapLPHP turns facebook's https://l.facebook.com/l.php?u=<target> redirect into the target.
func unwrapLPHP(addr string) string {
	if addr == "" {
		return ""
	}
	if parsed, err := neturl.Parse(addr); err == nil && parsed.Path == "/l.php" {
		if u := parsed.Query().Get("u"); u != "" {
			return u
		}
	}
	return addr
}

// communityPicture: field 5 of the community row is the picture url (field 6 is a path).
func communityPicture(u string) string {
	if strings.HasPrefix(u, "http://") || strings.HasPrefix(u, "https://") {
		return u
	}
	return ""
}

// blobKind classifies a blob attachment by its type, falling back to the MIME type.
func blobKind(t table.AttachmentType, mime string) string {
	switch {
	case t == table.AttachmentTypeImage || t == table.AttachmentTypeAnimatedImage || strings.HasPrefix(mime, "image/"):
		return "image"
	case t == table.AttachmentTypeVideo || strings.HasPrefix(mime, "video/"):
		return "video"
	case t == table.AttachmentTypeAudio || strings.HasPrefix(mime, "audio/"):
		return "audio"
	case t == table.AttachmentTypeSticker:
		return "sticker"
	}
	return "file"
}

func blobLabel(t table.AttachmentType, mime string) string {
	switch blobKind(t, mime) {
	case "image":
		return "📷 Photo"
	case "video":
		return "🎬 Video"
	case "audio":
		return "🎤 Voice message"
	case "sticker":
		return "💟 Sticker"
	}
	return "📎 File"
}

func blobMedia(t table.AttachmentType, mime, playable, preview string) (kind, url string) {
	kind = blobKind(t, mime)
	switch kind {
	case "image", "sticker":
		url = preview
		if url == "" {
			url = playable
		}
	default:
		url = playable
	}
	return kind, url
}

type basicMsgMeta struct {
	thread int64
	sender int64
	ts     int64
}

var (
	msgMetaMu    sync.Mutex
	knownMsgMeta = map[string]basicMsgMeta{}
)

func putMsgMeta(id string, m basicMsgMeta) {
	if id == "" {
		return
	}
	msgMetaMu.Lock()
	if len(knownMsgMeta) > 10000 {
		knownMsgMeta = map[string]basicMsgMeta{}
	}
	knownMsgMeta[id] = m
	msgMetaMu.Unlock()
}

func getMsgMeta(id string) (basicMsgMeta, bool) {
	msgMetaMu.Lock()
	defer msgMetaMu.Unlock()
	m, ok := knownMsgMeta[id]
	return m, ok
}

func recordThreadMetaLocked(key int64, tt table.ThreadType, sg int64, folder string) {
	if key == 0 {
		return
	}
	jid := key
	if m := hybridFBToJID[key]; m != 0 {
		jid = m
	} else if m := cutoverOpenToE2EE[key]; m != 0 {
		jid = m
	}
	if tt != 0 {
		threadTypes[key] = tt
		threadTypes[jid] = tt
	}
	if tt >= 18 && tt <= 26 {
		threadSyncGroup[key] = 104
	} else if sg > 0 {
		threadSyncGroup[key] = sg
	}
	if tt.IsWhatsApp() || strings.Contains(folder, "e2ee_cutover") {
		e2eeKeys[jid] = true
		if tt == table.ENCRYPTED_OVER_WA_GROUP {
			e2eeGroups[fmtID(jid)] = true
		}
	}
}

func processTable(tbl *table.LSTable, live bool) {
	if tbl == nil {
		return
	}
	now := time.Now().UnixMilli()

	// 1. Pre-pass: record thread authority, hybrid FB↔JID mappings, cutover links,
	// SyncGroup assignments, and message pagination ranges BEFORE emitting any thread
	// or message events so every ID resolves to its canonical "e:<jid>" or "<fbid>".
	activeThreads := map[int64]bool{}
	var pendingMerges [][2]string

	metaMu.Lock()
	for _, c := range tbl.LSCreateOpenToE2EEThreadLink {
		if c.ArmadilloThreadID != 0 {
			e2eeKeys[c.ArmadilloThreadID] = true
			if c.OpenThreadID != 0 {
				cutoverOpenToE2EE[c.OpenThreadID] = c.ArmadilloThreadID
				cutoverE2EEToOpen[c.ArmadilloThreadID] = c.OpenThreadID
				if c.OpenThreadID != c.ArmadilloThreadID {
					pendingMerges = append(pendingMerges, [2]string{fmtID(c.OpenThreadID), "e:" + fmtID(c.ArmadilloThreadID)})
				} else {
					pendingMerges = append(pendingMerges, [2]string{fmtID(c.OpenThreadID), "e:" + fmtID(c.ArmadilloThreadID)})
				}
			}
		}
	}
	for _, j := range tbl.LSUpdateThreadAuthorityAndMappingWithOTIDFromJID {
		if j.ThreadJID != 0 {
			e2eeKeys[j.ThreadJID] = true
			activeThreads[j.ThreadJID] = true
			if j.ThreadKey != 0 {
				hybridFBToJID[j.ThreadKey] = j.ThreadJID
				hybridJIDToFB[j.ThreadJID] = j.ThreadKey
				threadSyncGroup[j.ThreadKey] = 95
				pendingMerges = append(pendingMerges, [2]string{fmtID(j.ThreadKey), "e:" + fmtID(j.ThreadJID)})
			}
		}
	}
	var hybridReads [][2]any
	for _, j := range tbl.LSVerifyHybridThreadExists {
		if j.ThreadJID != 0 {
			e2eeKeys[j.ThreadJID] = true
			activeThreads[j.ThreadJID] = true
			if j.ThreadType != 0 {
				threadTypes[j.ThreadJID] = j.ThreadType
			}
			if j.IsGroupThread || j.ThreadType == table.ENCRYPTED_OVER_WA_GROUP {
				e2eeGroups[fmtID(j.ThreadJID)] = true
			}
			if j.ThreadKey != 0 {
				hybridFBToJID[j.ThreadKey] = j.ThreadJID
				hybridJIDToFB[j.ThreadJID] = j.ThreadKey
				threadSyncGroup[j.ThreadKey] = 95
				activeThreads[j.ThreadKey] = true
				pendingMerges = append(pendingMerges, [2]string{fmtID(j.ThreadKey), "e:" + fmtID(j.ThreadJID)})
			}
			thID := "e:" + fmtID(j.ThreadJID)
			wm := threadWatermarks[thID]
			if j.LastReadWatermarkTimestampMS > wm.readUpTo {
				wm.readUpTo = j.LastReadWatermarkTimestampMS
			}
			if j.LastActivityTimestampMS > wm.lastActivity {
				wm.lastActivity = j.LastActivityTimestampMS
			}
			threadWatermarks[thID] = wm
			if j.LastReadWatermarkTimestampMS > 0 && j.LastReadWatermarkTimestampMS >= j.LastActivityTimestampMS {
				hybridReads = append(hybridReads, [2]any{thID, j.LastReadWatermarkTimestampMS})
			}
		}
	}
	for _, mv := range tbl.LSMoveThreadToE2EECutoverFolder {
		if mv.ThreadKey != 0 {
			_, jid, _ := resolveThreadLocked(mv.ThreadKey)
			e2eeKeys[jid] = true
			pendingMerges = append(pendingMerges, [2]string{fmtID(mv.ThreadKey), "e:" + fmtID(jid)})
		}
	}
	for _, v := range tbl.LSVerifyThreadExists {
		activeThreads[v.ThreadKey] = true
		recordThreadMetaLocked(v.ThreadKey, v.ThreadType, v.SyncGroup, v.FolderName)
	}
	for _, th := range tbl.LSDeleteThenInsertThread {
		activeThreads[th.ThreadKey] = true
		recordThreadMetaLocked(th.ThreadKey, th.ThreadType, th.SyncGroup, th.FolderName)
	}
	for _, th := range tbl.LSUpdateOrInsertThread {
		activeThreads[th.ThreadKey] = true
		recordThreadMetaLocked(th.ThreadKey, th.ThreadType, th.SyncGroup, th.FolderName)
	}
	metaMu.Unlock()

	for _, rng := range tbl.LSInsertNewMessageRange {
		activeThreads[rng.ThreadKey] = true
	}
	for _, mg := range pendingMerges {
		if mg[0] != "" && mg[1] != "" && mg[0] != mg[1] {
			emit(map[string]any{"type": "mergeThread", "from": mg[0], "to": mg[1]})
		}
	}
	for _, hr := range hybridReads {
		emit(map[string]any{"type": "read", "thread": hr[0], "readUpTo": hr[1]})
	}

	// 2. Contacts, participants, presence.
	emitContact := func(id int64, name, first, avatar string) {
		if id == 0 {
			return
		}
		emit(map[string]any{
			"type": "contact", "id": fmtID(id), "name": name,
			"firstName": first, "avatar": avatar,
		})
	}
	for _, ct := range tbl.LSDeleteThenInsertContact {
		emitContact(ct.Id, ct.Name, ct.FirstName, firstNonEmpty(ct.ProfilePictureUrl, ct.ProfilePictureFallbackUrl))
	}
	for _, ct := range tbl.LSVerifyContactRowExists {
		emitContact(ct.ContactId, ct.Name, ct.FirstName, firstNonEmpty(ct.ProfilePictureUrl, ct.ProfilePictureFallbackUrl))
	}
	for _, p := range tbl.LSAddParticipantIdToGroupThread {
		emit(map[string]any{
			"type": "participant", "thread": emitThreadID(p.ThreadKey),
			"contact": fmtID(p.ContactId), "nickname": p.Nickname,
			"admin": p.IsAdmin || p.IsSuperAdmin,
		})
	}
	for _, r := range tbl.LSRemoveParticipantFromThread {
		emit(map[string]any{
			"type": "removeParticipant", "thread": emitThreadID(r.ThreadKey),
			"contact": fmtID(r.ParticipantId),
		})
	}
	for _, a := range tbl.LSUpdateThreadParticipantAdminStatus {
		emit(map[string]any{
			"type": "admin", "thread": emitThreadID(a.ThreadKey),
			"contact": fmtID(a.ContactId), "admin": a.IsAdmin,
		})
	}
	for _, p := range tbl.LSDeleteThenInsertContactPresence {
		if p.ContactId == 0 {
			continue
		}
		online := p.ExpirationTimestampMs > now
		ev := map[string]any{
			"type": "presence", "id": fmtID(p.ContactId),
			"online": online,
		}
		if p.LastActiveTimestampMs > 0 {
			ev["lastSeen"] = float64(p.LastActiveTimestampMs)
		}
		emit(ev)
	}

	// 3. Message deletes, edits, reactions, typing, read receipts & thread read watermarks.
	for _, d := range tbl.LSDeleteMessage {
		if d.MessageId != "" {
			emit(map[string]any{"type": "delete", "thread": emitThreadID(d.ThreadKey), "id": d.MessageId})
		}
	}
	for _, ed := range tbl.LSEditMessage {
		if ed.MessageID != "" {
			emit(map[string]any{"type": "edit", "id": ed.MessageID, "text": ed.Text})
		}
	}
	for _, mr := range tbl.LSMarkThreadRead {
		if mr.ThreadKey != 0 && mr.LastReadWatermarkTimestampMs > 0 {
			thID := emitThreadID(mr.ThreadKey)
			noteThreadWatermark(thID, mr.LastReadWatermarkTimestampMs, 0)
			emit(map[string]any{"type": "read", "thread": thID, "readUpTo": mr.LastReadWatermarkTimestampMs})
		}
	}
	for _, mr := range tbl.LSMarkThreadReadV2 {
		if mr.ThreadKey != 0 && mr.LastReadWatermarkTimestampMs > 0 {
			thID := emitThreadID(mr.ThreadKey)
			noteThreadWatermark(thID, mr.LastReadWatermarkTimestampMs, 0)
			emit(map[string]any{"type": "read", "thread": thID, "readUpTo": mr.LastReadWatermarkTimestampMs})
		}
	}
	for _, b := range tbl.LSBumpThread {
		if b.ThreadKey != 0 && b.LastReadWatermarkTimestampMs > 0 &&
			(b.BumpStatus == table.ACTIVITY_AND_READ || b.BumpStatus == table.ACTIVITY_AND_READ_2) {
			thID := emitThreadID(b.ThreadKey)
			noteThreadWatermark(thID, b.LastReadWatermarkTimestampMs, 0)
			emit(map[string]any{"type": "read", "thread": thID, "readUpTo": b.LastReadWatermarkTimestampMs})
		}
	}
	for _, r := range tbl.LSUpdateReadReceipt {
		thID := emitThreadID(r.ThreadKey)
		reader := fmtID(r.ContactId)
		if r.ContactId == 0 || reader == selfFBID {
			if r.ReadWatermarkTimestampMs > 0 {
				noteThreadWatermark(thID, r.ReadWatermarkTimestampMs, 0)
				emit(map[string]any{"type": "read", "thread": thID, "readUpTo": r.ReadWatermarkTimestampMs})
			}
			continue
		}
		emit(map[string]any{
			"type": "receipt", "thread": thID,
			"status": "read", "ts": float64(r.ReadWatermarkTimestampMs),
		})
		ts := r.ReadActionTimestampMs
		if ts == 0 {
			ts = r.ReadWatermarkTimestampMs
		}
		emit(map[string]any{
			"type": "seen", "thread": thID,
			"contact": reader, "ts": float64(ts),
		})
	}
	for _, r := range tbl.LSUpsertReaction {
		emit(map[string]any{
			"type": "reaction", "thread": emitThreadID(r.ThreadKey),
			"id": r.MessageId, "emoji": r.Reaction, "actor": fmtID(r.ActorId),
		})
	}
	for _, r := range tbl.LSDeleteReaction {
		emit(map[string]any{
			"type": "reaction", "thread": emitThreadID(r.ThreadKey),
			"id": r.MessageId, "emoji": "", "actor": fmtID(r.ActorId),
		})
	}
	for _, ti := range tbl.LSUpdateTypingIndicator {
		emit(map[string]any{
			"type": "typing", "thread": emitThreadID(ti.ThreadKey),
			"id": fmtID(ti.SenderId), "composing": ti.IsTyping,
		})
	}

	// 4. Community folders, full thread rows, partial thread metadata updates, and thread deletions.
	for _, c := range tbl.LSPopulateCommunityFolderData {
		if c.ThreadKey <= 0 || c.Name == "" {
			continue
		}
		emit(map[string]any{
			"type": "community", "id": fmtID(c.ThreadKey), "name": c.Name,
			"description": c.Description, "members": c.MemberCount, "picture": communityPicture(c.Field5),
		})
	}
	for _, f := range tbl.LSUpsertFolder {
		if f.ThreadKey <= 0 || f.ThreadName == "" {
			continue
		}
		emit(map[string]any{
			"type": "community", "id": fmtID(f.ThreadKey), "name": f.ThreadName,
			"picture": f.ThreadPictureURL, "lastActivity": f.LastActivityTimestampMS,
		})
	}

	emitThreadRow := func(rawKey int64, name, snippet, pic, picFallback, folderName string, lastAct, readWm, muteExp, parentKey int64, ttype table.ThreadType) {
		if rawKey == 0 {
			return
		}
		metaMu.Lock()
		id, jid, encrypted := resolveThreadLocked(rawKey)
		isGrp := e2eeGroups[fmtID(jid)] || ttype == table.ENCRYPTED_OVER_WA_GROUP || ttype == table.GROUP_THREAD
		metaMu.Unlock()

		noteThreadWatermark(id, readWm, lastAct)
		if !strings.HasPrefix(pic, "https://") && strings.HasPrefix(picFallback, "https://") {
			pic = picFallback
		} else if pic == "" {
			pic = picFallback
		}
		folder := normalizeFolder(folderName)
		ev := map[string]any{
			"type":         "thread",
			"id":           id,
			"name":         name,
			"snippet":      snippet,
			"picture":      pic,
			"lastActivity": lastAct,
			"unread":       lastAct > readWm,
			"readUpTo":     readWm,
			"folder":       folder,
			"ttype":        int64(ttype),
			"parent":       fmtID(parentKey),
			"muted":        muteExp != 0 && (muteExp < 0 || muteExp > now),
			"e2ee":         encrypted,
		}
		if encrypted {
			if !isGrp {
				ev["contact"] = fmtID(jid)
			}
			if rawPlain := fmtID(rawKey); rawPlain != id {
				ev["mergeFrom"] = rawPlain
			}
		}
		noteOldestThread(rawKey, lastAct, parentKey, folder)
		emit(ev)
	}

	seenFullThread := map[int64]bool{}
	for _, th := range tbl.LSDeleteThenInsertThread {
		seenFullThread[th.ThreadKey] = true
		emitThreadRow(
			th.ThreadKey, th.ThreadName, th.Snippet, th.ThreadPictureUrl, th.ThreadPictureUrlFallback,
			th.FolderName, th.LastActivityTimestampMs, th.LastReadWatermarkTimestampMs,
			th.MuteExpireTimeMs, th.ParentThreadKey, th.ThreadType,
		)
	}
	for _, th := range tbl.LSUpdateOrInsertThread {
		if seenFullThread[th.ThreadKey] {
			continue
		}
		emitThreadRow(
			th.ThreadKey, th.ThreadName, th.Snippet, th.ThreadPictureUrl, th.ThreadPictureUrlFallback,
			th.FolderName, th.LastActivityTimestampMs, th.LastReadWatermarkTimestampMs,
			th.MuteExpireTimeMs, th.ParentThreadKey, th.ThreadType,
		)
	}
	for _, hybrid := range tbl.LSVerifyHybridThreadExists {
		if hybrid.ThreadJID == 0 || hybrid.LastActivityTimestampMS <= 0 {
			continue
		}
		id := "e:" + fmtID(hybrid.ThreadJID)
		event := map[string]any{
			"type": "thread", "id": id, "partial": true,
			"lastActivity": hybrid.LastActivityTimestampMS, "readUpTo": hybrid.LastReadWatermarkTimestampMS,
			"unread": hybrid.LastActivityTimestampMS > hybrid.LastReadWatermarkTimestampMS,
		}
		if !hybrid.IsGroupThread && hybrid.ThreadType != table.ENCRYPTED_OVER_WA_GROUP {
			event["contact"] = fmtID(hybrid.ThreadJID)
		}
		emit(event)
	}
	for _, u := range tbl.LSSyncUpdateThreadName {
		name := u.ThreadName
		if name == "" {
			name = u.ThreadName1
		}
		if u.ThreadKey != 0 && name != "" {
			emit(map[string]any{"type": "thread", "id": emitThreadID(u.ThreadKey), "partial": true, "name": name})
		}
	}
	for _, u := range tbl.LSSetThreadImageURL {
		pic := firstNonEmpty(u.ImageURL, u.ImageFallbackURL)
		if u.ThreadKey != 0 && pic != "" {
			emit(map[string]any{"type": "thread", "id": emitThreadID(u.ThreadKey), "partial": true, "picture": pic})
		}
	}
	for _, u := range tbl.LSUpdateThreadMuteSetting {
		if u.ThreadKey != 0 {
			muted := u.MuteExpireTimeMS != 0 && (u.MuteExpireTimeMS < 0 || u.MuteExpireTimeMS > now)
			emit(map[string]any{"type": "thread", "id": emitThreadID(u.ThreadKey), "partial": true, "muted": muted})
		}
	}
	for _, u := range tbl.LSMoveThreadToArchivedFolder {
		if u.ThreadKey != 0 {
			emit(map[string]any{"type": "thread", "id": emitThreadID(u.ThreadKey), "partial": true, "folder": "archived"})
		}
	}
	for _, u := range tbl.LSMoveThreadToInboxAndUpdateParent {
		if u.ThreadKey != 0 {
			emit(map[string]any{"type": "thread", "id": emitThreadID(u.ThreadKey), "partial": true, "folder": "inbox", "parent": fmtID(u.ParentThreadKey)})
		}
	}
	emitDeleteThread := func(rawKey int64) {
		if rawKey == 0 || activeThreads[rawKey] {
			return
		}
		metaMu.Lock()
		id, jid, _ := resolveThreadLocked(rawKey)
		metaMu.Unlock()
		if activeThreads[jid] {
			return
		}
		emit(map[string]any{"type": "deleteThread", "thread": id})
	}
	for _, d := range tbl.LSDeleteThread {
		emitDeleteThread(d.ThreadKey)
	}
	for _, d := range tbl.LSDeletePartialThread {
		emitDeleteThread(d.ThreadKey)
	}

	// 5. Index messages first so multi-attachment items (2nd, 3rd photo/file in the same
	// message) can synthesize distinct "#1", "#2" message rows instead of overwriting.
	for _, m := range tbl.LSInsertMessage {
		if !m.IsUnsent && m.MessageId != "" {
			putMsgMeta(m.MessageId, basicMsgMeta{thread: m.ThreadKey, sender: m.SenderId, ts: m.TimestampMs})
		}
	}
	for _, m := range tbl.LSUpsertMessage {
		if !m.IsUnsent && m.MessageId != "" {
			putMsgMeta(m.MessageId, basicMsgMeta{thread: m.ThreadKey, sender: m.SenderId, ts: m.TimestampMs})
		}
	}
	for _, m := range tbl.LSDeleteThenInsertMessage {
		if !m.IsUnsent && m.MessageId != "" {
			putMsgMeta(m.MessageId, basicMsgMeta{thread: m.ThreadKey, sender: m.SenderId, ts: m.TimestampMs})
		}
	}

	for _, a := range tbl.LSInsertStickerAttachment {
		if a.MessageId != "" {
			setAttachLabel(a.MessageId, "💟 Sticker")
		}
	}
	for _, a := range tbl.LSInsertXmaAttachment {
		if a.MessageId != "" && getAttachLabel(a.MessageId) == "" {
			setAttachLabel(a.MessageId, "🔗 Link")
		}
	}

	mediaCount := map[string]int{}
	seenAttachFbid := map[string]bool{}
	emitAttachmentPart := func(msgID, fbid string, threadKey int64, kind, url, label string) {
		if msgID == "" {
			return
		}
		if fbid != "" {
			dedupKey := msgID + "|" + fbid
			if seenAttachFbid[dedupKey] {
				return
			}
			seenAttachFbid[dedupKey] = true
		}
		idx := mediaCount[msgID]
		mediaCount[msgID] = idx + 1
		partID := msgID
		if idx == 0 {
			setAttachLabel(msgID, label)
		} else {
			partID = fmt.Sprintf("%s#%d", msgID, idx)
		}
		thID := emitThreadID(threadKey)
		if kind != "" && url != "" {
			emit(map[string]any{"type": "media", "id": partID, "thread": thID, "kind": kind, "url": url})
			if idx > 0 {
				if meta, ok := getMsgMeta(msgID); ok {
					emit(map[string]any{
						"type":   "message",
						"id":     partID,
						"thread": emitThreadID(meta.thread),
						"sender": fmtID(meta.sender),
						"text":   label,
						"ts":     float64(meta.ts) + float64(idx)*0.001,
						"live":   false,
						"system": false,
					})
				}
			}
		}
	}

	for _, a := range tbl.LSInsertBlobAttachment {
		preview := a.PreviewUrlLarge
		if preview == "" {
			preview = a.PreviewUrl
		}
		kind, url := blobMedia(a.AttachmentType, a.AttachmentMimeType, a.PlayableUrl, preview)
		label := blobLabel(a.AttachmentType, a.AttachmentMimeType)
		if a.Filename != "" && kind == "file" {
			label = "📎 " + a.Filename
		}
		emitAttachmentPart(a.MessageId, a.AttachmentFbid, a.ThreadKey, kind, url, label)
	}
	for _, a := range tbl.LSInsertAttachment {
		url := a.PreviewUrl
		if url == "" {
			url = a.PlayableUrl
		}
		kind := ""
		switch a.AttachmentType {
		case table.AttachmentTypeImage, table.AttachmentTypeAnimatedImage:
			kind = "image"
		case table.AttachmentTypeSticker:
			kind = "sticker"
		case table.AttachmentTypeVideo:
			kind, url = "video", a.PlayableUrl
		case table.AttachmentTypeAudio:
			kind, url = "audio", a.PlayableUrl
		case table.AttachmentTypeFile:
			kind, url = "file", a.PlayableUrl
		}
		label := attachTypeLabel(a.AttachmentType)
		if a.Filename != "" && kind == "file" {
			label = "📎 " + a.Filename
		}
		emitAttachmentPart(a.MessageId, a.AttachmentFbid, a.ThreadKey, kind, url, label)
	}

	// A link card's URL lives in its CTA ("open" button) row, often wrapped in facebook's l.php redirect.
	for _, cta := range tbl.LSInsertAttachmentCta {
		raw := cta.ActionUrl
		if raw == "" {
			raw = cta.NativeUrl
		}
		u := unwrapLPHP(raw)
		if u == "" || cta.MessageId == "" {
			continue
		}
		ctaURL.Store(cta.AttachmentFbid, u)
		emit(map[string]any{"type": "linkurl", "id": cta.MessageId, "thread": emitThreadID(cta.ThreadKey), "url": u})
	}
	for _, a := range tbl.LSInsertXmaAttachment {
		if a.MessageId == "" {
			continue
		}
		linkURL := unwrapLPHP(a.ActionUrl)
		if linkURL == "" {
			if v, ok := ctaURL.Load(a.AttachmentFbid); ok {
				linkURL, _ = v.(string)
			}
		}
		img := a.ImageUrl
		if img == "" {
			img = a.PreviewUrlLarge
		}
		if img == "" {
			img = a.PreviewUrl
		}
		if a.TitleText == "" && a.SubtitleText == "" && a.DescriptionText == "" && linkURL == "" && img == "" {
			continue
		}
		emit(map[string]any{
			"type": "link", "id": a.MessageId, "thread": emitThreadID(a.ThreadKey),
			"title": a.TitleText, "subtitle": a.SubtitleText, "description": a.DescriptionText,
			"source": a.SourceText, "url": linkURL, "image": img,
		})
	}
	for _, a := range tbl.LSInsertStickerAttachment {
		picture := firstNonEmpty(a.PreviewUrl, a.ImageUrl, a.PreviewUrlFallback, a.ImageUrlFallback)
		if strings.HasPrefix(a.PlayableUrlMimeType, "image/") && a.PlayableUrl != "" {
			picture = a.PlayableUrl
		}
		if a.MessageId != "" && picture != "" {
			noteSticker(a.MessageId, a.TargetId, picture, false)
			emitStickerPicture(a.MessageId, emitThreadID(a.ThreadKey), picture)
		}
	}
	for _, replacement := range tbl.LSReplaceOptimsiticMessage {
		attachSentSticker(replacement.OfflineThreadingId, replacement.MessageId, "")
	}

	emitMsg := func(id, otid string, thread, sender, ts, stickerID int64, text string, system bool, replyID, replyText, replySnippet string, replyTo, replyTs int64) {
		if id == "" {
			return
		}
		sentStickerID := attachSentSticker(otid, id, emitThreadID(thread))
		if stickerID == 0 {
			stickerID = sentStickerID
		}
		if text == "" {
			text = getAttachLabel(id)
		}
		if text == "" && stickerID != 0 {
			text = "💟 Sticker"
		}
		if text == "" && !system {
			text = "📎 Attachment"
		}
		if text == "" {
			return
		}
		thID := emitThreadID(thread)
		noteThreadWatermark(thID, 0, ts)
		ev := map[string]any{
			"type": "message", "id": id, "thread": thID, "sender": fmtID(sender),
			"text": text, "ts": ts, "live": live, "system": system,
		}
		if stickerID > 0 && likeText(stickerID, "") == "" {
			ev["stickerId"] = fmtID(stickerID)
		}
		if replyID != "" {
			ev["replyToId"] = replyID
			if replyTs > 0 {
				ev["replyToTs"] = replyTs
			}
			quoted := replyText
			if quoted == "" {
				quoted = replySnippet
			}
			if quoted != "" {
				ev["replyToText"] = quoted
			}
			if replyTo != 0 {
				ev["replyToSender"] = fmtID(replyTo)
			}
		}
		emit(ev)
	}
	emitUnsent := func(id string, thread int64) {
		if id != "" {
			emit(map[string]any{"type": "delete", "thread": emitThreadID(thread), "id": id})
		}
	}
	for _, m := range tbl.LSInsertMessage {
		if m.IsUnsent {
			emitUnsent(m.MessageId, m.ThreadKey)
			continue
		}
		noteSticker(m.MessageId, m.StickerId, "", true)
		emitMsg(m.MessageId, m.OfflineThreadingId, m.ThreadKey, m.SenderId, m.TimestampMs, m.StickerId, likeText(m.StickerId, m.Text), m.IsAdminMessage,
			m.ReplySourceId, m.ReplyMessageText, m.ReplySnippet, m.ReplyToUserId, m.ReplySourceTimestampMs)
	}
	for _, m := range tbl.LSUpsertMessage {
		if m.IsUnsent {
			emitUnsent(m.MessageId, m.ThreadKey)
			continue
		}
		noteSticker(m.MessageId, m.StickerId, "", true)
		emitMsg(m.MessageId, m.OfflineThreadingId, m.ThreadKey, m.SenderId, m.TimestampMs, m.StickerId, likeText(m.StickerId, m.Text), m.IsAdminMessage,
			m.ReplySourceId, m.ReplyMessageText, m.ReplySnippet, m.ReplyToUserId, m.ReplySourceTimestampMs)
	}
	for _, m := range tbl.LSDeleteThenInsertMessage {
		if m.IsUnsent {
			emitUnsent(m.MessageId, m.ThreadKey)
			continue
		}
		noteSticker(m.MessageId, m.StickerId, "", true)
		emitMsg(m.MessageId, m.OfflineThreadingId, m.ThreadKey, m.SenderId, m.TimestampMs, m.StickerId, likeText(m.StickerId, m.Text), m.IsAdminMessage,
			m.ReplySourceId, m.ReplyMessageText, m.ReplySnippet, m.ReplyToUserId, m.ReplySourceTimestampMs)
	}
	for _, rng := range tbl.LSInsertNewMessageRange {
		noteMessageRange(rng)
	}
}

// sendMediaMessage reads a picked file and sends it as media.
func sendMediaMessage(threadStr, path, caption string) {
	data, err := os.ReadFile(path)
	if err != nil {
		emit(map[string]any{"type": "error", "msg": "read file: " + err.Error()})
		return
	}
	mime := http.DetectContentType(data)
	kind := "file"
	switch {
	case strings.HasPrefix(mime, "image/"):
		kind = "image"
	case strings.HasPrefix(mime, "video/"):
		kind = "video"
	case strings.HasPrefix(mime, "audio/"):
		kind = "audio"
	}
	w, h := 0, 0
	if kind == "image" {
		if cfg, _, err := image.DecodeConfig(bytes.NewReader(data)); err == nil {
			w, h = cfg.Width, cfg.Height
		}
	}
	fileName := filepath.Base(path)
	if strings.HasPrefix(threadStr, "e:") {
		if err := sendE2EEMedia(strings.TrimPrefix(threadStr, "e:"), kind, data, mime, caption, fileName, w, h); err != nil {
			log.Warn().Err(err).Str("kind", kind).Msg("E2EE media send failed")
			emit(map[string]any{"type": "error", "msg": "send media: " + err.Error()})
		}
		return
	}
	if err := sendMediaRegular(threadStr, data, mime, caption, fileName); err != nil {
		log.Warn().Err(err).Str("kind", kind).Msg("media send failed")
		emit(map[string]any{"type": "error", "msg": "send media: " + err.Error()})
	}
}

func sendMediaRegular(threadStr string, data []byte, mime, caption, fileName string) error {
	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil {
		return err
	}
	if client == nil {
		return fmt.Errorf("not connected")
	}
	if fileName == "" {
		fileName = "upload"
	}
	resp, err := client.GetHTTP().SendMercuryUploadRequest(context.Background(), tid, &httpclient.MercuryUploadMedia{
		Filename:  fileName,
		MimeType:  mime,
		MediaData: data,
	})
	if err != nil {
		return fmt.Errorf("upload: %w", err)
	}
	fbid := resp.Payload.RealMetadata.GetFbId()
	if fbid == 0 {
		return fmt.Errorf("no media id from upload")
	}
	task := &socket.SendMessageTask{
		ThreadId:         tid,
		Otid:             methods.GenerateEpochID(),
		Source:           table.MESSENGER_INBOX_IN_THREAD,
		InitiatingSource: table.FACEBOOK_INBOX,
		SendType:         table.MEDIA,
		SyncGroup:        syncGroupFor(tid),
		AttachmentFBIds:  []int64{fbid},
		Text:             caption,
	}
	_, err = runTasks(context.Background(), task)
	return err
}

func sendGif(threadStr, gifURL string) {
	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil || client == nil {
		return
	}
	task := &socket.SendMessageTask{
		ThreadId:         tid,
		Otid:             methods.GenerateEpochID(),
		Source:           table.MESSENGER_INBOX_IN_THREAD,
		InitiatingSource: table.FACEBOOK_INBOX,
		SendType:         table.EXTERNAL_MEDIA,
		SyncGroup:        syncGroupFor(tid),
		Url:              gifURL,
	}
	if _, err := runTasks(context.Background(), task); err != nil {
		emit(map[string]any{"type": "error", "msg": "gif: " + err.Error()})
	}
}

func unsendMessage(threadStr, msgID string) {
	if msgID == "" {
		return
	}
	if strings.HasPrefix(threadStr, "e:") {
		if err := sendE2EEUnsend(strings.TrimPrefix(threadStr, "e:"), msgID); err != nil {
			emit(map[string]any{"type": "error", "msg": "unsend: " + err.Error()})
		}
		return
	}
	if _, err := runTasks(context.Background(), &socket.DeleteMessageTask{MessageId: msgID}); err != nil {
		emit(map[string]any{"type": "error", "msg": "unsend: " + err.Error()})
	}
}

func editMessage(threadStr, msgID, text string) {
	if msgID == "" {
		return
	}
	if strings.HasPrefix(threadStr, "e:") {
		if err := sendE2EEEdit(strings.TrimPrefix(threadStr, "e:"), msgID, text); err != nil {
			emit(map[string]any{"type": "error", "msg": "edit: " + err.Error()})
		}
		return
	}
	if _, err := runTasks(context.Background(), &socket.EditMessageTask{MessageID: msgID, Text: text}); err != nil {
		emit(map[string]any{"type": "error", "msg": "edit: " + err.Error()})
	}
}

func sendReaction(threadStr, msgID, emoji string, fromMe bool) {
	if strings.HasPrefix(threadStr, "e:") {
		if err := sendE2EEReaction(strings.TrimPrefix(threadStr, "e:"), msgID, emoji, fromMe); err != nil {
			emit(map[string]any{"type": "error", "msg": "react: " + err.Error()})
		}
		return
	}
	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil {
		return
	}
	task := &socket.SendReactionTask{
		ThreadKey:       tid,
		MessageID:       msgID,
		ActorID:         toInt64(selfFBID),
		Reaction:        emoji,
		SyncGroup:       int(syncGroupFor(tid)),
		SendAttribution: table.MESSENGER_INBOX_IN_THREAD,
	}
	if _, err := runTasks(context.Background(), task); err != nil {
		emit(map[string]any{"type": "error", "msg": "react: " + err.Error()})
	}
}

// setMute mutes (forever) or unmutes a conversation.
func setMute(threadStr string, muted bool) {
	tid := toInt64(strings.TrimPrefix(threadStr, "e:"))
	if tid == 0 || client == nil {
		return
	}
	var expire int64 // 0 = unmute
	if muted {
		expire = 9223372036854775807 // max int64 → mute forever
	}
	target := tid
	sg := syncGroupFor(tid)
	if strings.HasPrefix(threadStr, "e:") {
		if fbKey := getHybridFBKey(tid); fbKey != 0 {
			target = fbKey
			sg = 95
		}
	}
	task := &socket.MuteThreadTask{ThreadKey: target, MailboxType: 0, MuteExpireTimeMS: expire, SyncGroup: sg}
	if _, err := runTasks(context.Background(), task); err != nil {
		log.Warn().Err(err).Msg("mute failed")
		emit(map[string]any{"type": "error", "msg": "mute: " + err.Error()})
	}
}

// deleteThread removes a whole conversation from the inbox (server-side).
func deleteThread(threadStr string) {
	tid := toInt64(strings.TrimPrefix(threadStr, "e:"))
	if tid == 0 || client == nil {
		return
	}
	target := tid
	sg := syncGroupFor(tid)
	if strings.HasPrefix(threadStr, "e:") {
		if fbKey := getHybridFBKey(tid); fbKey != 0 {
			target = fbKey
			sg = 95
		}
	}
	task := &socket.DeleteThreadTask{ThreadKey: target, RemoveType: 0, SyncGroup: sg}
	if _, err := runTasks(context.Background(), task); err != nil {
		log.Warn().Err(err).Msg("delete thread failed")
		emit(map[string]any{"type": "error", "msg": "delete: " + err.Error()})
	}
}

func groupKey(threadStr string) int64 { return toInt64(strings.TrimPrefix(threadStr, "e:")) }

func runTask(label string, task socket.Task) {
	if _, err := runTasks(context.Background(), task); err != nil {
		log.Warn().Err(err).Str("task", label).Msg("group task failed")
		emit(map[string]any{"type": "error", "msg": label + ": " + err.Error()})
	}
}

func renameThread(threadStr, name string) {
	if tid := groupKey(threadStr); tid != 0 {
		runTask("rename", &socket.RenameThreadTask{ThreadKey: tid, ThreadName: name, SyncGroup: syncGroupFor(tid)})
	}
}

func addMembers(threadStr string, idsAny any) {
	tid := groupKey(threadStr)
	if tid == 0 {
		return
	}
	var ids []int64
	if arr, ok := idsAny.([]any); ok {
		for _, v := range arr {
			if id := toInt64(v); id != 0 {
				ids = append(ids, id)
			}
		}
	}
	if len(ids) > 0 {
		runTask("addMembers", &socket.AddParticipantsTask{ThreadKey: tid, ContactIDs: ids, SyncGroup: syncGroupFor(tid)})
	}
}

func removeMember(threadStr, idStr string) {
	tid, cid := groupKey(threadStr), toInt64(idStr)
	if tid != 0 && cid != 0 {
		runTask("removeMember", &socket.RemoveParticipantTask{ThreadID: tid, ContactID: cid})
	}
}

func setThreadAdmin(threadStr, idStr string, admin bool) {
	tid, cid := groupKey(threadStr), toInt64(idStr)
	if tid == 0 || cid == 0 {
		return
	}
	isAdmin := 0
	if admin {
		isAdmin = 1
	}
	runTask("setAdmin", &socket.UpdateAdminTask{ThreadKey: tid, ContactID: cid, IsAdmin: isAdmin})
}

func leaveThread(threadStr string) {
	tid, cid := groupKey(threadStr), toInt64(selfFBID)
	if tid != 0 && cid != 0 {
		runTask("leave", &socket.RemoveParticipantTask{ThreadID: tid, ContactID: cid})
	}
}

func setGroupPhoto(threadStr, path string) {
	tid := groupKey(threadStr)
	if tid == 0 || client == nil {
		return
	}
	data, err := os.ReadFile(path)
	if err != nil {
		emit(map[string]any{"type": "error", "msg": "photo: " + err.Error()})
		return
	}
	resp, err := client.GetHTTP().SendMercuryUploadRequest(context.Background(), tid, &httpclient.MercuryUploadMedia{
		Filename:  "avatar.jpg",
		MimeType:  http.DetectContentType(data),
		MediaData: data,
	})
	if err != nil {
		emit(map[string]any{"type": "error", "msg": "photo upload: " + err.Error()})
		return
	}
	imageID := resp.Payload.RealMetadata.GetFbId()
	if imageID == 0 {
		emit(map[string]any{"type": "error", "msg": "photo: no image id from upload"})
		return
	}
	runTask("setPhoto", &socket.SetThreadImageTask{ThreadKey: tid, ImageID: imageID, SyncGroup: syncGroupFor(tid)})
}

func createGroup(name string, idsAny any) {
	if client == nil {
		return
	}
	var ids []int64
	if arr, ok := idsAny.([]any); ok {
		for _, v := range arr {
			if id := toInt64(v); id != 0 {
				ids = append(ids, id)
			}
		}
	}
	if len(ids) == 0 {
		return
	}
	otid := methods.GenerateEpochID()
	task := &socket.CreateGroupTask{
		Participants: ids,
		SendPayload: socket.CreateGroupPayload{
			ThreadID: 0, OTID: strconv.FormatInt(otid, 10), Source: 0, SendType: 8,
		},
	}
	tbl, err := runTasks(context.Background(), task)
	if err != nil {
		emit(map[string]any{"type": "error", "msg": "create group: " + err.Error()})
		return
	}
	if name = strings.TrimSpace(name); name != "" && tbl != nil {
		for _, th := range tbl.LSDeleteThenInsertThread {
			if th.ThreadKey != 0 {
				go renameThread(fmtID(th.ThreadKey), name)
			}
		}
		for _, th := range tbl.LSUpdateOrInsertThread {
			if th.ThreadKey != 0 {
				go renameThread(fmtID(th.ThreadKey), name)
			}
		}
	}
}

// markReadServer synchronizes the read state with Meta's servers on BOTH WhatsApp E2EE
// (for "Seen" receipts on companion devices) and Lightspeed (SyncGroup 95 / 1 / 104,
// which drives LastReadWatermarkTimestampMs when the app restarts).
func markReadServer(threadStr, msgID string, msgIDs []string, sender string, tsMs int64) {
	wm := getThreadWatermark(threadStr)
	if wm.lastActivity > tsMs {
		tsMs = wm.lastActivity
	}
	if tsMs <= 0 {
		tsMs = time.Now().UnixMilli()
	}
	// WhatsApp message timestamps have 1-second resolution (000ms), whereas Lightspeed's
	// LastActivityTimestampMs has millisecond precision; add 999ms so the watermark is >= activity.
	if tsMs%1000 == 0 {
		tsMs += 999
	}
	noteThreadWatermark(threadStr, tsMs, 0)

	if strings.HasPrefix(threadStr, "e:") {
		raw := strings.TrimPrefix(threadStr, "e:")
		allIDs := make([]string, 0, len(msgIDs)+1)
		seen := map[string]bool{}
		for _, id := range msgIDs {
			if id != "" && !seen[id] {
				seen[id] = true
				allIDs = append(allIDs, id)
			}
		}
		if msgID != "" && !seen[msgID] {
			allIDs = append(allIDs, msgID)
		}
		if len(allIDs) > 0 {
			markE2EERead(raw, allIDs, sender, tsMs)
		}

		// Also update Lightspeed's thread read watermark (SyncGroup 95 hybrid row, JID row,
		// and any linked pre-cutover open thread in SyncGroup 1) so restarting the app
		// won't reload an old LastReadWatermarkTimestampMs < LastActivityTimestampMs!
		jid := toInt64(raw)
		if jid != 0 && client != nil {
			var tasks []socket.Task
			added := map[[2]int64]bool{}
			addMark := func(tk, sg int64) {
				if tk == 0 || sg == 0 {
					return
				}
				k := [2]int64{tk, sg}
				if !added[k] {
					added[k] = true
					tasks = append(tasks, &socket.ThreadMarkReadTask{
						ThreadId:            tk,
						LastReadWatermarkTs: tsMs,
						SyncGroup:           sg,
					})
				}
			}
			if fbKey := getHybridFBKey(jid); fbKey != 0 {
				addMark(fbKey, 95)
			}
			sg := syncGroupFor(jid)
			addMark(jid, sg)
			if sg != 95 {
				addMark(jid, 95)
			}
			if openID := getOpenThreadForE2EE(jid); openID != 0 {
				addMark(openID, 1)
			}
			if len(tasks) > 0 {
				if _, err := runTasks(context.Background(), tasks...); err != nil {
					log.Debug().Err(err).Str("thread", threadStr).Msg("E2EE LS mark read failed")
				}
			}
		}
		return
	}

	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil || tid == 0 {
		return
	}
	sg := syncGroupFor(tid)
	task := &socket.ThreadMarkReadTask{
		ThreadId:            tid,
		LastReadWatermarkTs: tsMs,
		SyncGroup:           sg,
	}
	if _, err := runTasks(context.Background(), task); err != nil {
		log.Debug().Err(err).Str("thread", threadStr).Msg("mark read failed")
	}
}

type mentionSpan struct {
	ID     string
	Offset int
	Length int
	Kind   string // "p" = person, "t" = @everyone / thread
}

func parseMentionSpans(v any) []mentionSpan {
	arr, ok := v.([]any)
	if !ok || len(arr) == 0 {
		return nil
	}
	out := make([]mentionSpan, 0, len(arr))
	for _, item := range arr {
		obj, ok := item.(map[string]any)
		if !ok {
			continue
		}
		id, _ := obj["id"].(string)
		kind, _ := obj["kind"].(string)
		off := int(toInt64(obj["offset"]))
		ln := int(toInt64(obj["length"]))
		if ln <= 0 || off < 0 {
			continue
		}
		if kind == "" {
			kind = "p"
		}
		out = append(out, mentionSpan{ID: id, Offset: off, Length: ln, Kind: kind})
	}
	return out
}

func sendMessage(threadStr, text, replyID, replySender, clientTag string, mentions []mentionSpan) {
	if strings.HasPrefix(threadStr, "e:") {
		sendE2EE(strings.TrimPrefix(threadStr, "e:"), text, replyID, replySender, clientTag, mentions)
		return
	}
	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil {
		return
	}
	otid := methods.GenerateEpochID()
	otidStr := strconv.FormatInt(otid, 10)
	task := &socket.SendMessageTask{
		ThreadId:         tid,
		Otid:             otid,
		Source:           table.MESSENGER_INBOX_IN_THREAD,
		InitiatingSource: table.FACEBOOK_INBOX,
		SendType:         table.TEXT,
		SyncGroup:        syncGroupFor(tid),
		Text:             text,
	}
	if len(mentions) > 0 {
		var sm socket.Mentions
		for _, ms := range mentions {
			if ms.Kind == "t" {
				sm = append(sm, socket.Mention{
					ID:     tid,
					Offset: ms.Offset,
					Length: ms.Length,
					Type:   socket.MentionTypeThread,
				})
			} else if uid := toInt64(ms.ID); uid != 0 {
				sm = append(sm, socket.Mention{
					ID:     uid,
					Offset: ms.Offset,
					Length: ms.Length,
					Type:   socket.MentionTypePerson,
				})
			}
		}
		if len(sm) > 0 {
			task.MentionData = sm.ToData()
		}
	}
	if replyID != "" {
		task.ReplyMetaData = &socket.ReplyMetaData{ReplyMessageId: replyID, ReplySourceType: 1, ReplyType: 0}
	}
	tbl, err := runTasks(context.Background(), task)
	if err != nil {
		log.Warn().Err(err).Msg("send failed")
		emit(map[string]any{"type": "sendFailed", "thread": threadStr, "clientTag": clientTag, "msg": "send failed: " + err.Error()})
		return
	}
	if tbl != nil {
		for _, f := range tbl.LSMarkOptimisticMessageFailed {
			if f.OTID == "" || f.OTID == otidStr {
				msg := f.Message
				if msg == "" {
					msg = "server rejected message"
				}
				emit(map[string]any{"type": "sendFailed", "thread": threadStr, "clientTag": clientTag, "msg": msg})
				return
			}
		}
		for _, f := range tbl.LSHandleFailedTask {
			if f.OTID == otidStr {
				msg := f.Message
				if msg == "" {
					msg = "task failed on server"
				}
				emit(map[string]any{"type": "sendFailed", "thread": threadStr, "clientTag": clientTag, "msg": msg})
				return
			}
		}
	}
}

var (
	requestedContacts   = map[int64]bool{}
	requestedContactsMu sync.Mutex
)

func fetchContact(id int64) {
	if id == 0 || client == nil {
		return
	}
	requestedContactsMu.Lock()
	already := requestedContacts[id]
	requestedContacts[id] = true
	requestedContactsMu.Unlock()
	if already {
		return
	}
	var lastErr error
	for attempt := 0; attempt < 6; attempt++ {
		_, err := runTasks(context.Background(), &socket.GetContactsFullTask{ContactID: id})
		if err == nil {
			return
		}
		lastErr = err
		time.Sleep(2 * time.Second)
	}
	log.Warn().Err(lastErr).Int64("id", id).Msg("fetchContact gave up")
	requestedContactsMu.Lock()
	delete(requestedContacts, id)
	requestedContactsMu.Unlock()
}

const (
	recentHistoryWindow = 7 * 24 * time.Hour
	olderHistoryWindow  = 3 * 24 * time.Hour
)

var refreshLocks sync.Map // string -> *sync.Mutex

func threadRefreshLock(threadStr string) *sync.Mutex {
	v, _ := refreshLocks.LoadOrStore(threadStr, &sync.Mutex{})
	return v.(*sync.Mutex)
}

// refreshThread pulls the newest messages for a thread and, when `sinceTs > 0`,
// paginates backwards up to `sinceTs` so messages missed while the app was closed
// never leave an unfillable hole in the middle of local history.
func refreshThread(threadStr string, sinceTs int64) {
	tid := toInt64(strings.TrimPrefix(threadStr, "e:"))
	if tid == 0 || client == nil {
		return
	}
	mu := threadRefreshLock(threadStr)
	if sinceTs <= 0 {
		if !mu.TryLock() {
			return
		}
	} else {
		mu.Lock()
	}
	defer mu.Unlock()

	if strings.HasPrefix(threadStr, "e:") {
		markE2EEKey(tid)
		if hasHistoryDevice() {
			refreshEncryptedHistory(threadStr, sinceTs)
		}
		// If this chat was upgraded from a non-E2EE chat and we have no messages yet,
		// also pull its pre-cutover Lightspeed messages so the chat isn't blank.
		if openID := getOpenThreadForE2EE(tid); openID != 0 && sinceTs <= 0 {
			_, _ = runTasks(context.Background(),
				&socket.CreateThreadTask{ThreadFBID: openID, SyncGroup: 1},
				&socket.FetchMessagesTask{
					ThreadKey:            openID,
					Direction:            0,
					ReferenceTimestampMs: time.Now().UnixMilli() + 60_000,
					ReferenceMessageId:   "",
					SyncGroup:            1,
					Cursor:               client.GetCursor(1),
				},
			)
		}
		return
	}

	maxPages := 1
	if sinceTs > 0 {
		maxPages = 12
		sinceTs = max(sinceTs, time.Now().Add(-recentHistoryWindow).UnixMilli())
	}
	if _, _, _, _, err := fetchOpenHistory(tid, sinceTs, maxPages, nil); err != nil {
		log.Warn().Err(err).Int64("thread", tid).Msg("refreshThread failed")
	}
}

func syncOpenHistory(thread, beforeID string, beforeTs int64, older bool) {
	result := map[string]any{"type": "openHistorySync", "thread": thread, "older": older}
	tid := toInt64(thread)
	if tid <= 0 || client == nil {
		result["state"] = "error"
		emit(result)
		return
	}

	mu := threadRefreshLock(thread)
	mu.Lock()
	defer mu.Unlock()

	cutoff := time.Now().Add(-recentHistoryWindow).UnixMilli()
	if older {
		base := time.Now()
		if beforeTs > 0 {
			base = time.UnixMilli(beforeTs)
		}
		cutoff = base.Add(-olderHistoryWindow).UnixMilli()
	}
	var before *table.LSInsertNewMessageRange
	if beforeID != "" && beforeTs > cutoff {
		before = &table.LSInsertNewMessageRange{MinMessageId: beforeID, MinTimestampMs: beforeTs}
	}
	pages, complete, exhausted, cursor, err := fetchOpenHistory(tid, cutoff, 500, before)
	state := "partial"
	switch {
	case err != nil:
		state = "error"
		log.Warn().Err(err).Int64("thread", tid).Int("pages", pages).Msg("syncOpenHistory failed")
	case exhausted:
		state = "exhausted"
	case complete:
		state = "done"
	case pages == 0:
		state = "unavailable"
	}
	result["state"] = state
	if !complete && !older && cursor != nil && cursor.MinMessageId != "" && cursor.MinTimestampMs > cutoff {
		result["beforeId"] = cursor.MinMessageId
		result["beforeTs"] = cursor.MinTimestampMs
	}
	emit(result)
	fmt.Fprintf(os.Stderr, "History sync for %s: %s (%d pages)\n", thread, state, pages)
}

// fetchOpenHistory walks confirmed server ranges. It stops at the requested
// timestamp or at the beginning of server history, whichever arrives first.
func fetchOpenHistory(tid, cutoff int64, maxPages int, before *table.LSInsertNewMessageRange) (pages int, complete, exhausted bool, cursor *table.LSInsertNewMessageRange, err error) {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	for pages < maxPages && ctx.Err() == nil {
		rangePage, err := requestOpenHistoryPage(ctx, tid, before)
		if err != nil {
			if ctx.Err() != nil {
				break
			}
			return pages, false, false, before, err
		}
		if rangePage == nil {
			break
		}
		moved := before == nil || rangeMovesBack(before, rangePage)
		sameAnchor := before != nil && before.MinMessageId == rangePage.MinMessageId && before.MinTimestampMs == rangePage.MinTimestampMs
		if (moved || sameAnchor) && !rangePage.HasMoreBefore {
			return pages + 1, true, true, nil, nil
		}
		if !moved || rangePage.MinMessageId == "" || rangePage.MinTimestampMs <= 0 {
			break
		}
		pages++
		if cutoff > 0 && rangePage.MinTimestampMs <= cutoff {
			return pages, true, false, nil, nil
		}
		before = rangePage
	}
	return pages, false, false, before, nil
}

func rangeMovesBack(previous, page *table.LSInsertNewMessageRange) bool {
	return page.MinTimestampMs < previous.MinTimestampMs ||
		(page.MinTimestampMs == previous.MinTimestampMs && page.MinMessageId != previous.MinMessageId)
}

func requestOpenHistoryPage(ctx context.Context, tid int64, before *table.LSInsertNewMessageRange) (*table.LSInsertNewMessageRange, error) {
	sg := syncGroupFor(tid)
	_, gen := getLastRange(tid)
	refTs := time.Now().UnixMilli() + 60_000
	var refID string
	if before == nil {
		if _, err := runTasks(ctx, &socket.CreateThreadTask{ThreadFBID: tid, SyncGroup: int(sg)}); err != nil {
			return nil, err
		}
		waitForRangeUpdate(tid, gen, 2*time.Second)
		if page, nextGen := getLastRange(tid); nextGen != gen {
			return page, nil
		}
	} else {
		refTs, refID = before.MinTimestampMs, before.MinMessageId
	}
	task := &socket.FetchMessagesTask{
		ThreadKey:            tid,
		Direction:            0,
		ReferenceTimestampMs: refTs,
		ReferenceMessageId:   refID,
		SyncGroup:            sg,
		Cursor:               client.GetCursor(cursorDBFor(sg)),
	}
	if _, err := runTasks(ctx, task); err != nil {
		return nil, err
	}
	waitForRangeUpdate(tid, gen, 8*time.Second)
	page, nextGen := getLastRange(tid)
	if nextGen == gen {
		return nil, nil
	}
	return page, nil
}

// Fetch the page containing the target without downloading the intervening history.
func fetchQuotedMessage(thread, id string, ts int64) {
	defer emit(map[string]any{"type": "quoteFetched", "thread": thread, "id": id})
	tid := toInt64(thread)
	if tid <= 0 || client == nil || id == "" || ts <= 0 {
		return
	}
	mu := threadRefreshLock(thread)
	mu.Lock()
	defer mu.Unlock()
	ctx, cancel := context.WithTimeout(context.Background(), 12*time.Second)
	defer cancel()
	_, _ = requestOpenHistoryPage(ctx, tid, &table.LSInsertNewMessageRange{
		MinMessageId:   id,
		MinTimestampMs: ts + 1,
	})
}

// loadEarlier backfills older messages before the oldest one the app currently has.
func loadEarlier(threadStr, oldestID string, oldestTs int64, fromMe bool) {
	_ = fromMe
	tid := toInt64(strings.TrimPrefix(threadStr, "e:"))
	if tid == 0 || client == nil {
		return
	}
	if strings.HasPrefix(threadStr, "e:") {
		markE2EEKey(tid)
		if openID := getOpenThreadForE2EE(tid); openID != 0 {
			tid = openID
		}
	}
	mu := threadRefreshLock(fmtID(tid))
	mu.Lock()
	defer mu.Unlock()
	sg := syncGroupFor(tid)
	cursorDB := cursorDBFor(sg)
	refTs := oldestTs
	if refTs <= 0 {
		refTs = time.Now().UnixMilli() + 60_000
	}
	var tasks []socket.Task
	if oldestID == "" {
		tasks = append(tasks, &socket.CreateThreadTask{
			ThreadFBID: tid,
			SyncGroup:  int(sg),
		})
	}
	tasks = append(tasks, &socket.FetchMessagesTask{
		ThreadKey:            tid,
		Direction:            0,
		ReferenceTimestampMs: refTs,
		ReferenceMessageId:   oldestID,
		SyncGroup:            sg,
		Cursor:               client.GetCursor(cursorDB),
	})
	if _, err := runTasks(context.Background(), tasks...); err != nil {
		log.Warn().Err(err).Int64("thread", tid).Msg("fetch messages failed")
		emit(map[string]any{"type": "error", "msg": "history: " + err.Error()})
		return
	}
	log.Info().Int64("thread", tid).Str("before", oldestID).Msg("requested earlier history")
}

// IDs are int64 up to ~2.5e16 — beyond float64's exact range — so they cross the
// JSON boundary as strings in both directions.
func fmtID(x int64) string { return strconv.FormatInt(x, 10) }

func toInt64(v any) int64 {
	switch n := v.(type) {
	case string:
		x, _ := strconv.ParseInt(n, 10, 64)
		return x
	case float64:
		return int64(n)
	case int64:
		return n
	}
	return 0
}

// likeText turns Facebook's thumbs-up sticker (the Like button) into the 👍 emoji.
func likeText(stickerID int64, text string) string {
	switch stickerID {
	case 369239263222822, 369239343222814, 369239383222810:
		return "👍"
	}
	return text
}

var (
	stickerMu   sync.Mutex
	stickerIDs  = map[string]int64{}
	stickerURLs = map[string]string{}
	stickerDone = map[string]bool{}
)

func noteSticker(msgID string, id int64, url string, fromMessage bool) {
	if msgID == "" {
		return
	}
	switch id {
	case 369239263222822, 369239343222814, 369239383222810:
		id = 0
	}
	stickerMu.Lock()
	defer stickerMu.Unlock()
	if id != 0 {
		if _, have := stickerIDs[msgID]; fromMessage || !have {
			stickerIDs[msgID] = id
		}
	}
	if url != "" {
		stickerURLs[msgID] = url
	}
	if stickerDone[msgID] || stickerIDs[msgID] == 0 || stickerURLs[msgID] == "" {
		return
	}
	stickerDone[msgID] = true
	if len(stickerDone) > 5000 {
		stickerDone = map[string]bool{}
		stickerIDs = map[string]int64{}
		stickerURLs = map[string]string{}
	}
	emit(map[string]any{"type": "stickerSeen", "stickerId": fmt.Sprint(stickerIDs[msgID]), "url": stickerURLs[msgID]})
}

type sentSticker struct {
	id      int64
	picture string
}

var sentStickers sync.Map

func attachSentSticker(otid, messageID, thread string) int64 {
	if value, ok := sentStickers.Load(otid); ok && messageID != "" {
		sticker := value.(sentSticker)
		emitStickerPicture(messageID, thread, sticker.picture)
		return sticker.id
	}
	return 0
}

func emitStickerPicture(messageID, thread, picture string) {
	emit(map[string]any{"type": "media", "id": messageID, "thread": thread, "kind": "sticker", "url": picture})
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func sendSticker(threadStr string, stickerID int64, picture string) {
	tid, err := strconv.ParseInt(threadStr, 10, 64)
	if err != nil || client == nil {
		emit(map[string]any{"type": "error", "msg": "stickers can only be sent in regular chats"})
		return
	}
	otid := methods.GenerateEpochID()
	key := fmtID(otid)
	if strings.HasPrefix(picture, "https://") {
		sentStickers.Store(key, sentSticker{stickerID, picture})
		time.AfterFunc(2*time.Minute, func() { sentStickers.Delete(key) })
	}
	task := &socket.SendMessageTask{
		ThreadId:         tid,
		Otid:             otid,
		Source:           table.MESSENGER_INBOX_IN_THREAD,
		InitiatingSource: table.FACEBOOK_INBOX,
		SendType:         table.STICKER,
		SyncGroup:        syncGroupFor(tid),
		StickerId:        stickerID,
	}
	if _, err := runTasks(context.Background(), task); err != nil {
		sentStickers.Delete(key)
		log.Error().Err(err).Msg("send sticker failed")
		emit(map[string]any{"type": "error", "msg": "sticker: " + err.Error()})
	}
}

var (
	threadsMu sync.Mutex
	oldestMu  sync.Mutex
	oldestKey int64
	oldestTs  int64
)

func noteOldestThread(key, ts, parent int64, folder string) {
	if key == 0 || ts <= 0 || folder != "inbox" || parent > 0 {
		return
	}
	oldestMu.Lock()
	defer oldestMu.Unlock()
	if oldestTs == 0 || ts < oldestTs {
		oldestKey, oldestTs = key, ts
	}
}

var recentInboxWake = make(chan struct{}, 1)

// Socket events remain the primary feed. Periodically refetch the head of both
// inbox transports to catch missed events; on reconnect also cover today's rows.
func watchRecentInbox() {
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()
	for {
		var pages int
		select {
		case <-recentInboxWake:
			pages = 12
		case <-ticker.C:
			pages = 1
		}
		ctx, cancel := context.WithTimeout(context.Background(), 25*time.Second)
		refreshRecentInbox(ctx, pages)
		cancel()
	}
}

func refreshRecentInbox(ctx context.Context, pages int) {
	const newest int64 = 9_999_999_999_999
	cutoff := time.Now().Add(-24 * time.Hour).UnixMilli()
	type anchor struct{ key, activity int64 }
	groups := []int{1, 95}
	anchors := map[int]anchor{1: {activity: newest}, 95: {activity: newest}}
	for page := 0; page < pages && len(anchors) > 0 && ctx.Err() == nil; page++ {
		tasks := make([]socket.Task, 0, len(anchors))
		for _, group := range groups {
			a, ok := anchors[group]
			if !ok {
				continue
			}
			task := &socket.FetchThreadsTask{
				ParentThreadKey:            -1,
				ReferenceThreadKey:         a.key,
				ReferenceActivityTimestamp: a.activity,
				SyncGroup:                  group,
			}
			if group == 1 {
				task.Cursor = client.GetCursor(1)
			}
			tasks = append(tasks, task)
		}
		tbl, err := runTasks(ctx, tasks...)
		if err != nil || tbl == nil {
			return
		}
		for _, group := range groups {
			prev, ok := anchors[group]
			if !ok {
				continue
			}
			next := prev
			for _, row := range tbl.LSUpsertSyncGroupThreadsRange {
				if row.SyncGroup != int64(group) || row.ParentThreadKey > 0 || !row.HasMoreBefore {
					continue
				}
				if row.MinThreadKey != 0 && row.MinLastActivityTimestampMS > cutoff &&
					(row.MinLastActivityTimestampMS < next.activity ||
						(row.MinLastActivityTimestampMS == next.activity && row.MinThreadKey < next.key)) {
					next = anchor{row.MinThreadKey, row.MinLastActivityTimestampMS}
				}
			}
			if next == prev {
				delete(anchors, group)
			} else {
				anchors[group] = next
			}
		}
	}
}

var threadBackfill atomic.Bool
var threadRowsSeen atomic.Int64

func loadMoreThreads(maxPages int) {
	if client == nil || !threadsMu.TryLock() {
		return
	}
	defer threadsMu.Unlock()
	threadBackfill.Store(true)
	defer func() {
		time.Sleep(3 * time.Second)
		threadBackfill.Store(false)
	}()
	for i := 0; i < maxPages; i++ {
		oldestMu.Lock()
		refKey, refTs := oldestKey, oldestTs
		oldestMu.Unlock()
		if refKey == 0 {
			return
		}
		task := &socket.FetchThreadsTask{
			IsAfter:                    0,
			ParentThreadKey:            0,
			ReferenceThreadKey:         refKey,
			ReferenceActivityTimestamp: refTs,
			AdditionalPagesToFetch:     0,
			Cursor:                     client.GetCursor(1),
			SyncGroup:                  1,
		}
		before := threadRowsSeen.Load()
		tbl, err := client.ExecuteTasks(context.Background(), task)
		for wait := 0; err != nil && strings.Contains(err.Error(), "not open") && wait < 10; wait++ {
			time.Sleep(3 * time.Second)
			tbl, err = client.ExecuteTasks(context.Background(), task)
		}
		if err != nil {
			log.Error().Err(err).Msg("fetch more threads failed")
			return
		}
		if tbl != nil {
			processTable(tbl, false)
			client.PostHandlePublishResponse(tbl)
		}
		last, quietSince := before, time.Now()
		for deadline := time.Now().Add(15 * time.Second); time.Now().Before(deadline); {
			time.Sleep(400 * time.Millisecond)
			if n := threadRowsSeen.Load(); n != last {
				last, quietSince = n, time.Now()
			} else if last > before && time.Since(quietSince) > 1500*time.Millisecond {
				break
			}
		}
		oldestMu.Lock()
		moved := oldestTs != refTs
		oldestMu.Unlock()
		if !moved {
			emit(map[string]any{"type": "threadsDone"})
			return
		}
		time.Sleep(500 * time.Millisecond)
	}
}
