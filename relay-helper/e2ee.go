package main

import (
	"context"
	"fmt"
	"io"
	"net/http"
	neturl "net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"go.mau.fi/mautrix-meta/pkg/messagix/methods"
	"go.mau.fi/mautrix-meta/pkg/messagix/table"
	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/armadilloutil"
	waArmadilloApplication "go.mau.fi/whatsmeow/proto/waArmadilloApplication"
	waArmadilloXMA "go.mau.fi/whatsmeow/proto/waArmadilloXMA"
	waCommon "go.mau.fi/whatsmeow/proto/waCommon"
	waConsumerApplication "go.mau.fi/whatsmeow/proto/waConsumerApplication"
	waE2E "go.mau.fi/whatsmeow/proto/waE2E"
	waMediaTransport "go.mau.fi/whatsmeow/proto/waMediaTransport"
	waMsgApplication "go.mau.fi/whatsmeow/proto/waMsgApplication"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/protobuf/proto"
	_ "modernc.org/sqlite"
)

var (
	e2eeClient      *whatsmeow.Client
	selfFBID        string
	e2eeReady       atomic.Bool
	e2eeOfflineSync atomic.Bool
	offlineSyncGen  atomic.Uint64
)

func e2eeChatJID(fbid string) types.JID {
	if isE2EEGroup(fbid) {
		return types.JID{User: fbid, Server: types.GroupServer}
	}
	return types.JID{User: fbid, Server: types.MessengerServer}
}

// setupE2EE connects the WhatsApp-protocol encrypted client that carries
// Messenger's E2EE (personal) chats. Registers a device once and persists it.
func setupE2EE(ctx context.Context, fbid int64) error {
	storeDir := dataDir()
	_ = os.MkdirAll(storeDir, 0o700)
	addr := "file:" + filepath.Join(storeDir, "wadevice.db") +
		"?_pragma=foreign_keys(1)&_pragma=busy_timeout(10000)"

	container, err := sqlstore.New(ctx, "sqlite", addr, waLog.Noop)
	if err != nil {
		return err
	}
	device, err := container.GetFirstDevice(ctx) // creates a fresh one if none
	if err != nil {
		return err
	}
	isNew := device.ID == nil
	if suggested := client.MessengerLite.GetSuggestedDeviceID(); suggested != uuid.Nil {
		device.FacebookUUID = suggested
	}
	client.SetDevice(device)

	if isNew {
		log.Info().Msg("registering new E2EE device (one-time)")
		if err := client.RegisterE2EE(ctx, fbid); err != nil {
			return err
		}
		if err := device.Save(ctx); err != nil {
			return err
		}
		log.Info().Msg("E2EE device registered")
	} else {
		log.Info().Msg("reusing saved E2EE device")
	}

	e2eeClient, err = client.PrepareE2EEClient()
	if err != nil {
		return err
	}
	// Reliability settings so encrypted messages aren't missed or lost:
	e2eeClient.EnableAutoReconnect = true                // heal transient socket drops
	e2eeClient.SynchronousAck = true                     // don't ack a message to the server…
	e2eeClient.EnableDecryptedEventBuffer = true         //   …until our handler has emitted it (no loss)
	e2eeClient.AutomaticMessageRerequestFromPhone = true // recover undecryptable messages from the phone

	e2eeClient.AddEventHandlerWithSuccessStatus(func(evt any) bool {
		switch e := evt.(type) {
		case *events.FBMessage: // Messenger E2EE messages (incl. offline + own-device)
			handleFBMessage(e)
		case *events.HistorySync:
			handleHistorySync(e)
		case *events.Presence: // online / last-seen
			handlePresence(e)
		case *events.ChatPresence: // typing indicator
			handleChatPresence(e)
		case *events.Receipt: // delivered / read
			handleReceipt(e)
		case *events.OfflineSyncPreview: // server is about to replay missed events
			log.Info().Int("messages", e.Messages).Int("total", e.Total).Msg("E2EE offline sync incoming")
			if e.Total > 0 || e.Messages > 0 {
				gen := offlineSyncGen.Add(1)
				e2eeOfflineSync.Store(true)
				time.AfterFunc(45*time.Second, func() {
					if offlineSyncGen.Load() == gen {
						e2eeOfflineSync.Store(false)
					}
				})
			}
		case *events.OfflineSyncCompleted:
			offlineSyncGen.Add(1)
			e2eeOfflineSync.Store(false)
			log.Info().Int("count", e.Count).Msg("E2EE offline sync completed")
		case *events.Connected:
			e2eeReady.Store(true)
			log.Info().Msg("E2EE socket connected")
			_ = e2eeClient.SendPresence(context.Background(), types.PresenceAvailable)
			go flushPendingE2EEReads()
		case *events.Disconnected:
			e2eeReady.Store(false)
			log.Warn().Msg("E2EE socket disconnected (auto-reconnecting)")
		case *events.LoggedOut:
			e2eeReady.Store(false)
			log.Error().Msg("E2EE logged out — session invalid")
			emit(map[string]any{"type": "error", "msg": "E2EE session expired — re-export cookies"})
		case *events.CATRefreshError: // auth token couldn't be refreshed in place
			log.Warn().Err(e.Error).Msg("E2EE auth token refresh failed — reloading session")
			go reloadE2EE()
		case *events.UndecryptableMessage:
			log.Warn().Str("chat", e.Info.Chat.User).Bool("unavailable", e.IsUnavailable).
				Msg("E2EE message failed to decrypt (auto-requesting resend)")
			handleUndecryptableMessage(e)
		default:
			log.Debug().Str("evt", fmt.Sprintf("%T", evt)).Msg("E2EE other event")
		}
		return true // we emit synchronously, so the message is safe to ack
	})
	if err := e2eeClient.Connect(); err != nil {
		return err
	}
	log.Info().Msg("✅ E2EE connected (encrypted personal chats)")
	return nil
}

func handleUndecryptableMessage(e *events.UndecryptableMessage) {
	if e == nil || e.DecryptFailMode == events.DecryptFailHide || e.Info.Chat.User == "" || e.Info.ID == "" {
		return
	}
	isGroup := e.Info.Chat.Server == types.GroupServer
	if isGroup {
		markE2EEGroup(e.Info.Chat.User)
		go fetchE2EEGroupInfo(e.Info.Chat)
	} else if fbid, err := strconv.ParseInt(e.Info.Chat.User, 10, 64); err == nil {
		markE2EEKey(fbid)
		go fetchContact(fbid)
	}
	thread := "e:" + e.Info.Chat.User
	sender := e.Info.Sender.User
	if e.Info.IsFromMe {
		sender = selfFBID
	}
	tsMs := e.Info.Timestamp.UnixMilli()
	emit(map[string]any{
		"type":   "message",
		"id":     e.Info.ID,
		"thread": thread,
		"sender": sender,
		"text":   "🔒 Tin nhắn mã hóa (đang đồng bộ…)",
		"ts":     float64(tsMs),
		"live":   false,
		"system": false,
	})
	// Also attempt recovery from Secure Storage after a short delay in case phone resend is offline.
	if hasHistoryDevice() {
		time.AfterFunc(4*time.Second, func() {
			refreshEncryptedHistory(thread, tsMs-5000)
		})
	}
}

var (
	e2eeReloadMu   sync.Mutex
	lastE2EEReload time.Time
)

func reloadE2EE() {
	e2eeReloadMu.Lock()
	defer e2eeReloadMu.Unlock()
	if !lastE2EEReload.IsZero() && time.Since(lastE2EEReload) < 30*time.Second {
		return
	}
	lastE2EEReload = time.Now()
	if _, _, err := client.LoadMessagesPage(context.Background()); err != nil {
		log.Warn().Err(err).Msg("session reload failed")
		return
	}
	if e2eeClient != nil {
		e2eeClient.Disconnect()
		if err := e2eeClient.Connect(); err != nil {
			log.Warn().Err(err).Msg("E2EE reconnect after reload failed")
		}
	}
	log.Info().Msg("E2EE session reloaded + reconnected")
}

var (
	subscribedPresence   = map[string]bool{}
	subscribedPresenceMu sync.Mutex

	fetchedGroups   = map[string]bool{}
	fetchedGroupsMu sync.Mutex
)

func fetchE2EEGroupInfo(jid types.JID) {
	if jid.User == "" || jid.Server != types.GroupServer {
		return
	}
	fetchedGroupsMu.Lock()
	if fetchedGroups[jid.User] {
		fetchedGroupsMu.Unlock()
		return
	}
	fetchedGroups[jid.User] = true
	fetchedGroupsMu.Unlock()

	for i := 0; i < 6; i++ {
		if e2eeClient != nil && e2eeReady.Load() {
			gi, err := e2eeClient.GetGroupInfo(context.Background(), jid)
			if err == nil && gi != nil {
				thread := "e:" + jid.User
				if gi.Name != "" {
					emit(map[string]any{"type": "thread", "id": thread, "partial": true, "name": gi.Name, "e2ee": true})
				}
				for _, p := range gi.Participants {
					if p.JID.User != "" {
						emit(map[string]any{
							"type": "participant", "thread": thread,
							"contact": p.JID.User, "nickname": "",
							"admin": p.IsAdmin || p.IsSuperAdmin,
						})
						if cid, err := strconv.ParseInt(p.JID.User, 10, 64); err == nil {
							go fetchContact(cid)
						}
					}
				}
				return
			}
		}
		time.Sleep(2 * time.Second)
	}
	fetchedGroupsMu.Lock()
	delete(fetchedGroups, jid.User)
	fetchedGroupsMu.Unlock()
}

func subscribePresence(fbid string) {
	if fbid == "" || isE2EEGroup(fbid) {
		return
	}
	subscribedPresenceMu.Lock()
	if subscribedPresence[fbid] {
		subscribedPresenceMu.Unlock()
		return
	}
	subscribedPresence[fbid] = true
	subscribedPresenceMu.Unlock()

	jid := types.JID{User: fbid, Server: types.MessengerServer}
	for i := 0; i < 15; i++ {
		if e2eeClient != nil && e2eeReady.Load() {
			if err := e2eeClient.SubscribePresence(context.Background(), jid); err == nil {
				return
			}
		}
		time.Sleep(2 * time.Second)
	}
	subscribedPresenceMu.Lock()
	delete(subscribedPresence, fbid)
	subscribedPresenceMu.Unlock()
}

func handlePresence(e *events.Presence) {
	if e.From.User == "" {
		return
	}
	out := map[string]any{"type": "presence", "id": e.From.User, "online": !e.Unavailable}
	if !e.LastSeen.IsZero() {
		out["lastSeen"] = float64(e.LastSeen.UnixMilli())
	}
	emit(out)
}

func handleChatPresence(e *events.ChatPresence) {
	if e.Chat.Server == types.GroupServer {
		markE2EEGroup(e.Chat.User)
	}
	emit(map[string]any{
		"type": "typing", "thread": "e:" + e.Chat.User, "id": e.Sender.User,
		"composing": e.State == types.ChatPresenceComposing,
	})
}

func handleReceipt(e *events.Receipt) {
	if e.Chat.User == "" {
		return
	}
	if e.Chat.Server == types.GroupServer {
		markE2EEGroup(e.Chat.User)
	}
	thread := "e:" + e.Chat.User
	tsMs := e.Timestamp.UnixMilli()

	// ReceiptTypeReadSelf means WE read this conversation on another device (phone/web).
	if e.Type == types.ReceiptTypeReadSelf {
		if tsMs <= 0 {
			tsMs = time.Now().UnixMilli()
		}
		noteThreadWatermark(thread, tsMs, 0)
		emit(map[string]any{"type": "read", "thread": thread, "readUpTo": tsMs})
		return
	}

	var status string
	switch e.Type {
	case types.ReceiptTypeRead:
		status = "read"
	case types.ReceiptTypeDelivered:
		status = "delivered"
	default:
		return
	}
	emit(map[string]any{
		"type": "receipt", "thread": thread,
		"status": status, "ts": float64(tsMs),
	})
	if e.Type == types.ReceiptTypeRead {
		reader := e.Sender.User
		if reader == "" {
			reader = e.Chat.User
		}
		if reader != selfFBID {
			emit(map[string]any{
				"type": "seen", "thread": thread,
				"contact": reader, "ts": float64(tsMs),
			})
		} else if tsMs > 0 {
			noteThreadWatermark(thread, tsMs, 0)
			emit(map[string]any{"type": "read", "thread": thread, "readUpTo": tsMs})
		}
	}
}

type pendingE2EERead struct {
	fbid   string
	msgIDs []string
	sender string
	tsMs   int64
}

var (
	pendingReadsMu sync.Mutex
	pendingReads   []pendingE2EERead
)

func stripPartSuffix(id string) string {
	if i := strings.IndexByte(id, '#'); i > 0 {
		return id[:i]
	}
	return id
}

func cleanMessageIDs(ids []string) []string {
	out := make([]string, 0, len(ids))
	seen := map[string]bool{}
	for _, raw := range ids {
		id := stripPartSuffix(raw)
		if id != "" && !seen[id] {
			seen[id] = true
			out = append(out, id)
		}
	}
	return out
}

func doMarkE2EERead(fbid string, msgIDs []string, sender string, tsMs int64) bool {
	if e2eeClient == nil || !e2eeReady.Load() {
		return false
	}
	cleanIDs := cleanMessageIDs(msgIDs)
	if len(cleanIDs) == 0 {
		return true
	}
	chat := e2eeChatJID(fbid)
	// Per WhatsApp multi-device protocol (and mautrix-meta handlematrix.go:597),
	// `sender` (the `participant` stanza attribute) MUST be EmptyJID in 1:1 DMs
	// and only populated in @g.us group chats.
	var snd types.JID
	if chat.Server == types.GroupServer && sender != "" {
		snd = types.JID{User: sender, Server: types.MessengerServer}
	}
	t := time.UnixMilli(tsMs)
	if tsMs <= 0 {
		t = time.Now()
	}
	if err := e2eeClient.MarkRead(context.Background(), cleanIDs, t, chat, snd); err != nil {
		log.Debug().Err(err).Str("chat", fbid).Msg("E2EE mark read failed")
		return false
	}
	return true
}

func markE2EERead(fbid string, msgIDs []string, sender string, tsMs int64) {
	cleanIDs := cleanMessageIDs(msgIDs)
	if fbid == "" || len(cleanIDs) == 0 {
		return
	}
	if doMarkE2EERead(fbid, cleanIDs, sender, tsMs) {
		return
	}
	pendingReadsMu.Lock()
	defer pendingReadsMu.Unlock()
	for i := range pendingReads {
		if pendingReads[i].fbid == fbid {
			pendingReads[i].msgIDs = cleanMessageIDs(append(pendingReads[i].msgIDs, cleanIDs...))
			if sender != "" {
				pendingReads[i].sender = sender
			}
			if tsMs > pendingReads[i].tsMs {
				pendingReads[i].tsMs = tsMs
			}
			return
		}
	}
	if len(pendingReads) >= 64 {
		pendingReads = pendingReads[1:]
	}
	pendingReads = append(pendingReads, pendingE2EERead{
		fbid: fbid, msgIDs: cleanIDs, sender: sender, tsMs: tsMs,
	})
}

func flushPendingE2EEReads() {
	pendingReadsMu.Lock()
	batch := pendingReads
	pendingReads = nil
	pendingReadsMu.Unlock()
	for _, pr := range batch {
		if !doMarkE2EERead(pr.fbid, pr.msgIDs, pr.sender, pr.tsMs) {
			markE2EERead(pr.fbid, pr.msgIDs, pr.sender, pr.tsMs)
		}
	}
}

func nextE2EEExtra() whatsmeow.SendRequestExtra {
	return whatsmeow.SendRequestExtra{
		ID: strconv.FormatInt(methods.GenerateEpochID(), 10),
	}
}

func sendE2EEReaction(fbid, targetMsgID, emoji string, targetFromMe bool) error {
	if e2eeClient == nil {
		return fmt.Errorf("E2EE not connected yet")
	}
	targetMsgID = stripPartSuffix(targetMsgID)
	chat := e2eeChatJID(fbid)
	rm := &waConsumerApplication.ConsumerApplication_ReactionMessage{
		Key: &waCommon.MessageKey{
			RemoteJID: proto.String(chat.String()),
			FromMe:    proto.Bool(targetFromMe),
			ID:        proto.String(targetMsgID),
		},
		Text:              proto.String(emoji),
		SenderTimestampMS: proto.Int64(time.Now().UnixMilli()),
	}
	app := &waConsumerApplication.ConsumerApplication{
		Payload: &waConsumerApplication.ConsumerApplication_Payload{
			Payload: &waConsumerApplication.ConsumerApplication_Payload_Content{
				Content: &waConsumerApplication.ConsumerApplication_Content{
					Content: &waConsumerApplication.ConsumerApplication_Content_ReactionMessage{ReactionMessage: rm},
				},
			},
		},
	}
	_, err := e2eeClient.SendFBMessage(context.Background(), chat, app, nil, nextE2EEExtra())
	return err
}

func sendE2EE(fbid, text, replyID, replySender, clientTag string, mentions []mentionSpan) {
	if e2eeClient == nil {
		emit(map[string]any{"type": "sendFailed", "thread": "e:" + fbid, "clientTag": clientTag, "msg": "E2EE not connected yet"})
		return
	}
	jid := e2eeChatJID(fbid)
	mt := &waCommon.MessageText{Text: proto.String(text)}
	if len(mentions) > 0 {
		seenJID := map[string]bool{}
		for _, ms := range mentions {
			if ms.Kind == "t" {
				mt.Commands = append(mt.Commands, &waCommon.Command{
					CommandType: waCommon.Command_EVERYONE.Enum(),
					Offset:      proto.Uint32(uint32(ms.Offset)),
					Length:      proto.Uint32(uint32(ms.Length)),
				})
			} else if ms.ID != "" {
				mjid := types.JID{User: ms.ID, Server: types.MessengerServer}.String()
				if !seenJID[mjid] {
					seenJID[mjid] = true
					mt.MentionedJID = append(mt.MentionedJID, mjid)
				}
				mt.Mentions = append(mt.Mentions, &waCommon.Mention{
					MentionType:  waCommon.Mention_PROFILE.Enum(),
					MentionedJID: proto.String(mjid),
					Offset:       proto.Uint32(uint32(ms.Offset)),
					Length:       proto.Uint32(uint32(ms.Length)),
				})
			}
		}
	}
	msg := &waConsumerApplication.ConsumerApplication{
		Payload: &waConsumerApplication.ConsumerApplication_Payload{
			Payload: &waConsumerApplication.ConsumerApplication_Payload_Content{
				Content: &waConsumerApplication.ConsumerApplication_Content{
					Content: &waConsumerApplication.ConsumerApplication_Content_MessageText{
						MessageText: mt,
					},
				},
			},
		},
	}
	var meta *waMsgApplication.MessageApplication_Metadata
	if replyID != "" {
		replyID = stripPartSuffix(replyID)
		sender := replySender
		if sender == "" {
			sender = fbid
		}
		meta = &waMsgApplication.MessageApplication_Metadata{
			QuotedMessage: &waMsgApplication.MessageApplication_Metadata_QuotedMessage{
				StanzaID:    proto.String(replyID),
				Participant: proto.String(types.JID{User: sender, Server: types.MessengerServer}.String()),
			},
		}
	}
	resp, err := e2eeClient.SendFBMessage(context.Background(), jid, msg, meta, nextE2EEExtra())
	if err != nil {
		log.Warn().Err(err).Msg("E2EE send failed")
		emit(map[string]any{"type": "sendFailed", "thread": "e:" + fbid, "clientTag": clientTag, "msg": "E2EE send: " + err.Error()})
		return
	}
	tsMs := resp.Timestamp.UnixMilli()
	if tsMs <= 0 {
		tsMs = time.Now().UnixMilli()
	}
	noteThreadWatermark("e:"+fbid, tsMs+999, tsMs)
	if clientTag != "" && resp.ID != "" {
		emit(map[string]any{"type": "sent", "clientTag": clientTag, "id": resp.ID, "thread": "e:" + fbid})
	}
}

func sendE2EEMedia(fbid, kind string, data []byte, mime, caption, fileName string, w, h int) error {
	if e2eeClient == nil {
		return fmt.Errorf("E2EE not connected yet")
	}
	var mediaType whatsmeow.MediaType
	switch kind {
	case "image":
		mediaType = whatsmeow.MediaImage
	case "video":
		mediaType = whatsmeow.MediaVideo
	case "audio":
		mediaType = whatsmeow.MediaAudio
	default:
		mediaType = whatsmeow.MediaDocument
	}
	uploaded, err := e2eeClient.Upload(context.Background(), data, mediaType)
	if err != nil {
		return fmt.Errorf("upload: %w", err)
	}
	if w == 0 {
		w, h = 400, 400
	}
	mt := &waMediaTransport.WAMediaTransport{
		Integral: &waMediaTransport.WAMediaTransport_Integral{
			FileSHA256:        uploaded.FileSHA256,
			MediaKey:          uploaded.MediaKey,
			FileEncSHA256:     uploaded.FileEncSHA256,
			DirectPath:        &uploaded.DirectPath,
			MediaKeyTimestamp: proto.Int64(time.Now().Unix()),
		},
		Ancillary: &waMediaTransport.WAMediaTransport_Ancillary{
			FileLength: proto.Uint64(uint64(len(data))),
			Mimetype:   &mime,
			Thumbnail: &waMediaTransport.WAMediaTransport_Ancillary_Thumbnail{
				ThumbnailWidth:  proto.Uint32(uint32(w)),
				ThumbnailHeight: proto.Uint32(uint32(h)),
			},
			ObjectID: &uploaded.ObjectID,
		},
	}
	var cap *waCommon.MessageText
	if caption != "" {
		cap = &waCommon.MessageText{Text: proto.String(caption)}
	}
	var content waConsumerApplication.ConsumerApplication_Content_Content
	displayText := caption
	saveExt := "bin"
	switch kind {
	case "video":
		saveExt = "mp4"
		if displayText == "" {
			displayText = "🎬 Video"
		}
		m := &waConsumerApplication.ConsumerApplication_VideoMessage{Caption: cap}
		if err := m.Set(&waMediaTransport.VideoTransport{
			Integral:  &waMediaTransport.VideoTransport_Integral{Transport: mt},
			Ancillary: &waMediaTransport.VideoTransport_Ancillary{Height: proto.Uint32(uint32(h)), Width: proto.Uint32(uint32(w))},
		}); err != nil {
			return err
		}
		content = &waConsumerApplication.ConsumerApplication_Content_VideoMessage{VideoMessage: m}
	case "audio":
		saveExt = "m4a"
		if displayText == "" {
			displayText = "🎤 Voice message"
		}
		ptt := true
		m := &waConsumerApplication.ConsumerApplication_AudioMessage{PTT: &ptt}
		if err := m.Set(&waMediaTransport.AudioTransport{
			Integral:  &waMediaTransport.AudioTransport_Integral{Transport: mt},
			Ancillary: &waMediaTransport.AudioTransport_Ancillary{},
		}); err != nil {
			return err
		}
		content = &waConsumerApplication.ConsumerApplication_Content_AudioMessage{AudioMessage: m}
	case "file":
		if i := strings.LastIndex(fileName, "."); i >= 0 && i < len(fileName)-1 {
			saveExt = fileName[i+1:]
		}
		if displayText == "" {
			if fileName != "" {
				displayText = "📎 " + fileName
			} else {
				displayText = "📎 File"
			}
		}
		fn := fileName
		m := &waConsumerApplication.ConsumerApplication_DocumentMessage{FileName: &fn}
		if err := m.Set(&waMediaTransport.DocumentTransport{
			Integral:  &waMediaTransport.DocumentTransport_Integral{Transport: mt},
			Ancillary: &waMediaTransport.DocumentTransport_Ancillary{},
		}); err != nil {
			return err
		}
		content = &waConsumerApplication.ConsumerApplication_Content_DocumentMessage{DocumentMessage: m}
	default:
		kind = "image"
		saveExt = sniffImageExt(data)
		if displayText == "" {
			displayText = "📷 Photo"
		}
		m := &waConsumerApplication.ConsumerApplication_ImageMessage{Caption: cap}
		if err := m.Set(&waMediaTransport.ImageTransport{
			Integral:  &waMediaTransport.ImageTransport_Integral{Transport: mt},
			Ancillary: &waMediaTransport.ImageTransport_Ancillary{Height: proto.Uint32(uint32(h)), Width: proto.Uint32(uint32(w))},
		}); err != nil {
			return err
		}
		content = &waConsumerApplication.ConsumerApplication_Content_ImageMessage{ImageMessage: m}
	}
	app := &waConsumerApplication.ConsumerApplication{
		Payload: &waConsumerApplication.ConsumerApplication_Payload{
			Payload: &waConsumerApplication.ConsumerApplication_Payload_Content{
				Content: &waConsumerApplication.ConsumerApplication_Content{Content: content},
			},
		},
	}
	jid := e2eeChatJID(fbid)
	resp, err := e2eeClient.SendFBMessage(context.Background(), jid, app, nil, nextE2EEExtra())
	if err != nil {
		return err
	}
	// Server does NOT echo E2EE sends back to the sending device — emit locally so
	// sent E2EE photos/videos/files appear immediately and persist in SQLite.
	msgID := resp.ID
	if msgID == "" {
		msgID = strconv.FormatInt(methods.GenerateEpochID(), 10)
	}
	tsMs := resp.Timestamp.UnixMilli()
	if tsMs <= 0 {
		tsMs = time.Now().UnixMilli()
	}
	thread := "e:" + fbid
	noteThreadWatermark(thread, tsMs+999, tsMs)
	emit(map[string]any{
		"type":   "message",
		"id":     msgID,
		"thread": thread,
		"sender": selfFBID,
		"text":   displayText,
		"ts":     float64(tsMs),
		"live":   false,
		"system": false,
	})
	if p := saveMedia(msgID, saveExt, data); p != "" {
		emit(map[string]any{"type": "media", "id": msgID, "thread": thread, "kind": kind, "path": p})
	}
	emit(map[string]any{
		"type": "thread", "id": thread, "partial": true,
		"snippet": displayText, "lastActivity": float64(tsMs), "unread": false, "e2ee": true,
	})
	return nil
}

func sendE2EEEdit(fbid, msgID, text string) error {
	if e2eeClient == nil {
		return fmt.Errorf("E2EE not connected yet")
	}
	msgID = stripPartSuffix(msgID)
	chat := e2eeChatJID(fbid)
	em := &waConsumerApplication.ConsumerApplication_EditMessage{
		Key: &waCommon.MessageKey{
			RemoteJID: proto.String(chat.String()),
			FromMe:    proto.Bool(true),
			ID:        proto.String(msgID),
		},
		Message: &waCommon.MessageText{Text: proto.String(text)},
	}
	app := &waConsumerApplication.ConsumerApplication{
		Payload: &waConsumerApplication.ConsumerApplication_Payload{
			Payload: &waConsumerApplication.ConsumerApplication_Payload_Content{
				Content: &waConsumerApplication.ConsumerApplication_Content{
					Content: &waConsumerApplication.ConsumerApplication_Content_EditMessage{EditMessage: em},
				},
			},
		},
	}
	_, err := e2eeClient.SendFBMessage(context.Background(), chat, app, nil, nextE2EEExtra())
	return err
}

func sendE2EEUnsend(fbid, msgID string) error {
	if e2eeClient == nil {
		return fmt.Errorf("E2EE not connected yet")
	}
	msgID = stripPartSuffix(msgID)
	chat := e2eeChatJID(fbid)
	app := &waConsumerApplication.ConsumerApplication{
		Payload: &waConsumerApplication.ConsumerApplication_Payload{
			Payload: &waConsumerApplication.ConsumerApplication_Payload_ApplicationData{
				ApplicationData: &waConsumerApplication.ConsumerApplication_ApplicationData{
					ApplicationContent: &waConsumerApplication.ConsumerApplication_ApplicationData_Revoke{
						Revoke: &waConsumerApplication.ConsumerApplication_RevokeMessage{
							Key: &waCommon.MessageKey{
								RemoteJID: proto.String(chat.String()),
								FromMe:    proto.Bool(true),
								ID:        proto.String(msgID),
							},
						},
					},
				},
			},
		},
	}
	_, err := e2eeClient.SendFBMessage(context.Background(), chat, app, nil, nextE2EEExtra())
	return err
}

func msgText(m *waE2E.Message) string {
	if m == nil {
		return ""
	}
	if t := m.GetConversation(); t != "" {
		return t
	}
	if e := m.GetExtendedTextMessage(); e != nil && e.GetText() != "" {
		return e.GetText()
	}
	if im := m.GetImageMessage(); im != nil {
		if im.GetCaption() != "" {
			return im.GetCaption()
		}
		return "📷 Photo"
	}
	if vm := m.GetVideoMessage(); vm != nil {
		if vm.GetCaption() != "" {
			return vm.GetCaption()
		}
		return "🎬 Video"
	}
	if dm := m.GetDocumentMessage(); dm != nil {
		if dm.GetCaption() != "" {
			return dm.GetCaption()
		}
		if dm.GetFileName() != "" {
			return "📎 " + dm.GetFileName()
		}
		return "📎 File"
	}
	if m.GetAudioMessage() != nil {
		return "🎤 Voice message"
	}
	if m.GetStickerMessage() != nil {
		return "💟 Sticker"
	}
	return ""
}

type e2eeMediaPart struct {
	kind  string
	label string
	dl    func() ([]byte, string, error)
}

type e2eeLinkCard struct {
	title    string
	subtitle string
	url      string
	thumbDL  func() ([]byte, string, error)
}

type extractedE2EE struct {
	text       string
	replyToID  string
	replyToUsr string
	link       *e2eeLinkCard
	media      []e2eeMediaPart
	skip       bool
}

func overrideTransportIntegral(tr *waMediaTransport.WAMediaTransport, pathOverrides []map[string]string) *waMediaTransport.WAMediaTransport_Integral {
	if tr == nil {
		return nil
	}
	in := tr.GetIntegral()
	if len(pathOverrides) > 0 && pathOverrides[0] != nil && in != nil {
		if path := pathOverrides[0][tr.GetAncillary().GetObjectID()]; path != "" {
			in = proto.CloneOf(in)
			in.DirectPath = proto.String(path)
		}
	}
	return in
}

func extractConsumerContent(rawContent *waConsumerApplication.ConsumerApplication_Content, pathOverrides []map[string]string) extractedE2EE {
	var out extractedE2EE
	if rawContent == nil {
		return out
	}
	switch c := rawContent.GetContent().(type) {
	case *waConsumerApplication.ConsumerApplication_Content_MessageText:
		out.text = c.MessageText.GetText()
	case *waConsumerApplication.ConsumerApplication_Content_ExtendedTextMessage:
		out.text = c.ExtendedTextMessage.GetText().GetText()
	case *waConsumerApplication.ConsumerApplication_Content_ImageMessage:
		out.text = c.ImageMessage.GetCaption().GetText()
		if out.text == "" {
			out.text = "📷 Photo"
		}
		imgMsg := c.ImageMessage
		out.media = append(out.media, e2eeMediaPart{
			kind: "image", label: "📷 Photo",
			dl: func() ([]byte, string, error) {
				t, err := imgMsg.Decode()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaImage)
				if err != nil {
					return nil, "jpg", err
				}
				return data, sniffImageExt(data), nil
			},
		})
	case *waConsumerApplication.ConsumerApplication_Content_VideoMessage:
		out.text = c.VideoMessage.GetCaption().GetText()
		if out.text == "" {
			out.text = "🎬 Video"
		}
		vidMsg := c.VideoMessage
		out.media = append(out.media, e2eeMediaPart{
			kind: "video", label: "🎬 Video",
			dl: func() ([]byte, string, error) {
				t, err := vidMsg.Decode()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaVideo)
				return data, "mp4", err
			},
		})
	case *waConsumerApplication.ConsumerApplication_Content_AudioMessage:
		out.text = "🎤 Voice message"
		audMsg := c.AudioMessage
		out.media = append(out.media, e2eeMediaPart{
			kind: "audio", label: "🎤 Voice message",
			dl: func() ([]byte, string, error) {
				t, err := audMsg.Decode()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaAudio)
				return data, "m4a", err
			},
		})
	case *waConsumerApplication.ConsumerApplication_Content_StickerMessage:
		out.text = "💟 Sticker"
		stkMsg := c.StickerMessage
		out.media = append(out.media, e2eeMediaPart{
			kind: "sticker", label: "💟 Sticker",
			dl: func() ([]byte, string, error) {
				t, err := stkMsg.Decode()
				if err != nil {
					return nil, "", err
				}
				if rfID := t.GetAncillary().GetReceiverFetchID(); rfID != "" {
					data, err := fetchReceiverFetchSticker(rfID)
					if err != nil {
						return nil, "", err
					}
					return data, sniffImageExt(data), nil
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaImage)
				if err != nil {
					return data, "webp", err
				}
				return data, sniffImageExt(data), nil
			},
		})
	case *waConsumerApplication.ConsumerApplication_Content_DocumentMessage:
		fn := c.DocumentMessage.GetFileName()
		label := "📎 File"
		if fn != "" {
			label = "📎 " + fn
		}
		out.text = label
		docMsg := c.DocumentMessage
		out.media = append(out.media, e2eeMediaPart{
			kind: "file", label: label,
			dl: func() ([]byte, string, error) {
				t, err := docMsg.Decode()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaDocument)
				ext := "bin"
				if i := strings.LastIndex(fn, "."); i >= 0 && i < len(fn)-1 {
					ext = fn[i+1:]
				}
				return data, ext, err
			},
		})
	case *waConsumerApplication.ConsumerApplication_Content_ViewOnceMessage:
		switch vo := c.ViewOnceMessage.GetViewOnceContent().(type) {
		case *waConsumerApplication.ConsumerApplication_ViewOnceMessage_ImageMessage:
			out.text = vo.ImageMessage.GetCaption().GetText()
			if out.text == "" {
				out.text = "📷 Photo"
			}
			imgMsg := vo.ImageMessage
			out.media = append(out.media, e2eeMediaPart{
				kind: "image", label: "📷 Photo",
				dl: func() ([]byte, string, error) {
					t, err := imgMsg.Decode()
					if err != nil {
						return nil, "", err
					}
					data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaImage)
					if err != nil {
						return nil, "jpg", err
					}
					return data, sniffImageExt(data), nil
				},
			})
		case *waConsumerApplication.ConsumerApplication_ViewOnceMessage_VideoMessage:
			out.text = vo.VideoMessage.GetCaption().GetText()
			if out.text == "" {
				out.text = "🎬 Video"
			}
			vidMsg := vo.VideoMessage
			out.media = append(out.media, e2eeMediaPart{
				kind: "video", label: "🎬 Video",
				dl: func() ([]byte, string, error) {
					t, err := vidMsg.Decode()
					if err != nil {
						return nil, "", err
					}
					data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(t.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaVideo)
					return data, "mp4", err
				},
			})
		default:
			out.text = "📷 Tin nhắn xem 1 lần"
		}
	case *waConsumerApplication.ConsumerApplication_Content_LocationMessage:
		loc := c.LocationMessage
		parts := []string{}
		if n := loc.GetLocation().GetName(); n != "" {
			parts = append(parts, n)
		}
		if a := loc.GetAddress(); a != "" {
			parts = append(parts, a)
		}
		if len(parts) > 0 {
			out.text = "📍 " + strings.Join(parts, " — ")
		} else {
			out.text = "📍 Location"
		}
	case *waConsumerApplication.ConsumerApplication_Content_LiveLocationMessage:
		out.text = "📍 Live location"
	case *waConsumerApplication.ConsumerApplication_Content_ContactMessage,
		*waConsumerApplication.ConsumerApplication_Content_ContactsArrayMessage:
		out.text = "👤 Contact"
	case *waConsumerApplication.ConsumerApplication_Content_PollCreationMessage:
		p := c.PollCreationMessage
		var sb strings.Builder
		sb.WriteString("📊 ")
		if p.GetName() != "" {
			sb.WriteString(p.GetName())
		} else {
			sb.WriteString("Bình chọn")
		}
		for _, opt := range p.GetOptions() {
			if on := opt.GetOptionName(); on != "" {
				sb.WriteString("\n• ")
				sb.WriteString(on)
			}
		}
		out.text = sb.String()
	case *waConsumerApplication.ConsumerApplication_Content_PollUpdateMessage,
		*waConsumerApplication.ConsumerApplication_Content_ReactionMessage,
		*waConsumerApplication.ConsumerApplication_Content_EditMessage:
		out.skip = true
	}
	return out
}

func extractArmadilloRaven(rm *waArmadilloApplication.Armadillo_Content_RavenMessage, pathOverrides []map[string]string) extractedE2EE {
	var out extractedE2EE
	if rm == nil {
		return out
	}
	switch rm.GetMediaContent().(type) {
	case *waArmadilloApplication.Armadillo_Content_RavenMessage_ImageMessage:
		out.text = "📷 Photo"
		out.media = append(out.media, e2eeMediaPart{
			kind: "image", label: "📷 Photo",
			dl: func() ([]byte, string, error) {
				img, err := rm.DecodeImageMessage()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(img.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaImage)
				if err != nil {
					return nil, "jpg", err
				}
				return data, sniffImageExt(data), nil
			},
		})
	case *waArmadilloApplication.Armadillo_Content_RavenMessage_VideoMessage:
		out.text = "🎬 Video"
		out.media = append(out.media, e2eeMediaPart{
			kind: "video", label: "🎬 Video",
			dl: func() ([]byte, string, error) {
				vid, err := rm.DecodeVideoMessage()
				if err != nil {
					return nil, "", err
				}
				data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(vid.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaVideo)
				return data, "mp4", err
			},
		})
	default:
		out.text = "📷 Tin nhắn xem 1 lần"
	}
	return out
}

func extractArmadilloExtended(xma *waArmadilloXMA.ExtendedContentMessage, pathOverrides []map[string]string) extractedE2EE {
	var out extractedE2EE
	if xma == nil {
		return out
	}
	var linkURL string
	for _, cta := range xma.GetCtas() {
		for _, cand := range []string{cta.GetNativeURL(), cta.GetActionURL()} {
			if cand == "" {
				continue
			}
			if parsed, err := neturl.Parse(cand); err == nil {
				if parsed.Scheme == "messenger" && parsed.Host == "location_share" {
					lbl := strings.TrimSpace(xma.GetTitleText() + " " + xma.GetSubtitleText())
					if lbl == "" {
						lbl = "Location"
					}
					out.text = "📍 " + lbl
					return out
				}
				if (parsed.Scheme == "https" || parsed.Scheme == "http") && parsed.Host != "" && linkURL == "" {
					linkURL = unwrapLPHP(cand)
				}
			}
		}
	}

	if xma.GetTargetType() == waArmadilloXMA.ExtendedContentMessage_FB_STORY_REPLY && xma.AssociatedMessage != nil {
		var assocMsg waMsgApplication.MessageApplication
		if _, err := armadilloutil.Unmarshal(&assocMsg, xma.AssociatedMessage, 2); err == nil {
			var consMsg waConsumerApplication.ConsumerApplication
			if _, err := armadilloutil.Unmarshal(&consMsg, assocMsg.GetPayload().GetSubProtocol().GetConsumerMessage(), 1); err == nil {
				inner := extractConsumerContent(consMsg.GetPayload().GetContent(), pathOverrides)
				if inner.text != "" {
					inner.text = "↪ Story: " + inner.text
					return inner
				}
			}
		}
	}

	var thumbDL func() ([]byte, string, error)
	for _, rawPrev := range xma.GetPreviews() {
		imgTr, err := armadilloutil.Unmarshal(&waMediaTransport.ImageTransport{}, rawPrev, 2)
		if err != nil || imgTr == nil {
			continue
		}
		tr := imgTr.GetIntegral().GetTransport()
		in := overrideTransportIntegral(tr, pathOverrides)
		if in == nil || in.GetDirectPath() == "" {
			continue
		}
		inCopy := in
		thumbDL = func() ([]byte, string, error) {
			data, err := e2eeClient.DownloadFB(context.Background(), inCopy, whatsmeow.MediaImage)
			if err != nil {
				return nil, "jpg", err
			}
			return data, sniffImageExt(data), nil
		}
		break
	}

	title := strings.TrimSpace(xma.GetTitleText())
	subtitle := strings.TrimSpace(xma.GetSubtitleText())
	if title != "" || subtitle != "" || linkURL != "" || thumbDL != nil {
		out.link = &e2eeLinkCard{
			title:    title,
			subtitle: subtitle,
			url:      linkURL,
			thumbDL:  thumbDL,
		}
	}
	out.text = strings.TrimSpace(xma.GetMessageText())
	if out.text == "" {
		if out.link != nil {
			out.text = "🔗 Link"
		} else if title != "" {
			out.text = title
		} else if subtitle != "" {
			out.text = subtitle
		} else {
			out.text = "🔗 Nội dung được chia sẻ"
		}
	}
	return out
}

// extractE2EE decodes both ConsumerApplication AND Armadillo E2EE payloads (links,
// URL previews, shared reels/posts/stories, multi-photo galleries, view-once media,
// polls, captions, bumped messages) so no E2EE message type is silently dropped.
func extractE2EE(e *events.FBMessage, pathOverrides ...map[string]string) extractedE2EE {
	var out extractedE2EE
	if e == nil {
		out.skip = true
		return out
	}

	switch typed := e.Message.(type) {
	case *waConsumerApplication.ConsumerApplication:
		if typed.GetPayload().GetApplicationData().GetRevoke() != nil {
			out.skip = true
			return out
		}
		out = extractConsumerContent(typed.GetPayload().GetContent(), pathOverrides)
	case *waArmadilloApplication.Armadillo:
		if e.Info.IsFromMe {
			if ms := typed.GetPayload().GetApplicationData().GetMetadataSync(); ms != nil {
				for _, action := range ms.GetActions() {
					ca := action.GetChatAction()
					if ca != nil && ca.GetChatDelete() != nil {
						raw := strings.SplitN(ca.GetChatID(), "@", 2)[0]
						if raw != "" {
							emit(map[string]any{"type": "deleteThread", "thread": "e:" + raw})
						}
					}
				}
				out.skip = true
				return out
			}
		}
		ac := typed.GetPayload().GetContent()
		if ac == nil || ac.GetRavenActionNotifMessage() != nil {
			out.skip = true
			return out
		}
		switch content := ac.GetContent().(type) {
		case *waArmadilloApplication.Armadillo_Content_ExtendedContentMessage:
			out = extractArmadilloExtended(content.ExtendedContentMessage, pathOverrides)
		case *waArmadilloApplication.Armadillo_Content_BumpExistingMessage_:
			out.text = "↪ Đã nhắc lại tin nhắn"
			if k := content.BumpExistingMessage.GetKey(); k != nil {
				out.replyToID = k.GetID()
				if p := k.GetParticipant(); p != "" {
					out.replyToUsr = strings.SplitN(p, "@", 2)[0]
				}
			}
		case *waArmadilloApplication.Armadillo_Content_RavenMessage_:
			out = extractArmadilloRaven(content.RavenMessage, pathOverrides)
		case *waArmadilloApplication.Armadillo_Content_RavenMessageMsgr:
			out = extractArmadilloRaven(content.RavenMessageMsgr, pathOverrides)
		case *waArmadilloApplication.Armadillo_Content_ImageGalleryMessage_:
			out.text = "📷 Photo"
			if imgs, err := content.ImageGalleryMessage.Decode(); err == nil {
				for _, img := range imgs {
					imgTr := img
					out.media = append(out.media, e2eeMediaPart{
						kind: "image", label: "📷 Photo",
						dl: func() ([]byte, string, error) {
							data, err := e2eeClient.DownloadFB(context.Background(), overrideTransportIntegral(imgTr.GetIntegral().GetTransport(), pathOverrides), whatsmeow.MediaImage)
							if err != nil {
								return nil, "jpg", err
							}
							return data, sniffImageExt(data), nil
						},
					})
				}
			}
		default:
			out.text = "📎 Tin nhắn đa phương tiện"
		}
	default:
		if e.Message == nil {
			out.skip = true
			return out
		}
		out.text = "📎 Tin nhắn"
	}

	if e.FBApplication != nil {
		if q := e.FBApplication.GetMetadata().GetQuotedMessage(); q != nil {
			if sid := q.GetStanzaID(); sid != "" {
				out.replyToID = sid
				if p := q.GetParticipant(); p != "" {
					out.replyToUsr = strings.SplitN(p, "@", 2)[0]
				}
			}
		}
	}
	return out
}

func emitExtractedE2EE(thread, baseID, sender string, tsMs int64, live bool, ext extractedE2EE, sem chan struct{}) {
	if baseID == "" || ext.text == "" {
		return
	}
	msgEv := map[string]any{
		"type": "message", "id": baseID, "thread": thread, "sender": sender,
		"text": ext.text, "ts": float64(tsMs), "live": live, "system": false,
	}
	if ext.replyToID != "" {
		msgEv["replyToId"] = ext.replyToID
		if ext.replyToUsr != "" {
			msgEv["replyToSender"] = ext.replyToUsr
		}
	}
	emit(msgEv)

	if ext.link != nil {
		lc := ext.link
		emit(map[string]any{
			"type": "link", "id": baseID, "thread": thread,
			"title": lc.title, "subtitle": lc.subtitle, "description": "",
			"source": "", "url": lc.url, "image": "",
		})
		if lc.thumbDL != nil {
			dl := lc.thumbDL
			go func() {
				if sem != nil {
					sem <- struct{}{}
					defer func() { <-sem }()
				}
				if e2eeClient == nil {
					return
				}
				data, fileExt, err := dl()
				if err != nil || len(data) == 0 {
					return
				}
				if p := saveMedia(baseID+"_preview", fileExt, data); p != "" {
					emit(map[string]any{
						"type": "link", "id": baseID, "thread": thread,
						"title": lc.title, "subtitle": lc.subtitle, "description": "",
						"source": "", "url": lc.url, "image": p,
					})
				}
			}()
		}
	}

	for i, mp := range ext.media {
		partID := baseID
		if i > 0 {
			partID = fmt.Sprintf("%s#%d", baseID, i)
			lbl := mp.label
			if lbl == "" {
				lbl = "📷 Photo"
			}
			emit(map[string]any{
				"type": "message", "id": partID, "thread": thread, "sender": sender,
				"text": lbl, "ts": float64(tsMs) + float64(i)*0.001, "live": false, "system": false,
			})
		}
		if mp.dl != nil {
			kind, dl, pid, th := mp.kind, mp.dl, partID, thread
			go func() {
				if sem != nil {
					sem <- struct{}{}
					defer func() { <-sem }()
				}
				if e2eeClient == nil {
					return
				}
				data, fileExt, err := dl()
				if err != nil || len(data) == 0 {
					log.Warn().Err(err).Str("kind", kind).Msg("media download failed")
					return
				}
				if p := saveMedia(pid, fileExt, data); p != "" {
					emit(map[string]any{"type": "media", "id": pid, "thread": th, "kind": kind, "path": p})
				}
			}()
		}
	}
}

// fetchReceiverFetchSticker downloads a Facebook-library sticker via messagix.
func fetchReceiverFetchSticker(id string) ([]byte, error) {
	if client == nil {
		return nil, fmt.Errorf("messagix client not ready")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancel()
	media, err := client.FetchReceiverFetchMedia(ctx, id)
	if err != nil {
		return nil, err
	}
	if media == nil || media.URL == "" {
		return nil, fmt.Errorf("receiver fetch returned no URL")
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, media.URL, nil)
	if err != nil {
		return nil, err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("sticker download HTTP %d", resp.StatusCode)
	}
	return io.ReadAll(io.LimitReader(resp.Body, 20<<20))
}

func sniffImageExt(b []byte) string {
	switch {
	case len(b) >= 8 && string(b[:8]) == "\x89PNG\r\n\x1a\n":
		return "png"
	case len(b) >= 6 && (string(b[:6]) == "GIF87a" || string(b[:6]) == "GIF89a"):
		return "gif"
	case len(b) >= 12 && string(b[:4]) == "RIFF" && string(b[8:12]) == "WEBP":
		return "webp"
	case len(b) >= 3 && b[0] == 0xFF && b[1] == 0xD8 && b[2] == 0xFF:
		return "jpg"
	}
	return "webp"
}

func saveMedia(id, ext string, data []byte) string {
	mdir := filepath.Join(dataDir(), "media")
	_ = os.MkdirAll(mdir, 0o700)
	safe := strings.NewReplacer("/", "_", ":", "_", "$", "_", ".", "_", "#", "_").Replace(id)
	p := filepath.Join(mdir, safe+"."+ext)
	if err := os.WriteFile(p, data, 0o600); err != nil {
		log.Warn().Err(err).Msg("save media failed")
		return ""
	}
	return p
}

func handleFBMessage(e *events.FBMessage) {
	if e.Info.Chat.User == "" {
		return
	}
	isGroup := e.Info.Chat.Server == types.GroupServer
	if isGroup {
		markE2EEGroup(e.Info.Chat.User)
	}
	thread := "e:" + e.Info.Chat.User

	// Unsend / delete: the partner (or you, from another device) removed a message.
	if rev := e.GetConsumerApplication().GetPayload().GetApplicationData().GetRevoke(); rev != nil {
		if id := rev.GetKey().GetID(); id != "" {
			emit(map[string]any{"type": "delete", "thread": thread, "id": id})
		}
		return
	}
	// Reaction add (emoji) / remove (empty) on one of our (or their) messages.
	if c, ok := e.GetConsumerApplication().GetPayload().GetContent().GetContent().(*waConsumerApplication.ConsumerApplication_Content_ReactionMessage); ok {
		rm := c.ReactionMessage
		actor := e.Info.Sender.User
		if e.Info.IsFromMe {
			actor = selfFBID
		}
		emit(map[string]any{
			"type": "reaction", "thread": thread,
			"id": rm.GetKey().GetID(), "emoji": rm.GetText(), "actor": actor,
		})
		return
	}
	// Edit: the author changed a sent message's text.
	if c, ok := e.GetConsumerApplication().GetPayload().GetContent().GetContent().(*waConsumerApplication.ConsumerApplication_Content_EditMessage); ok {
		if id := c.EditMessage.GetKey().GetID(); id != "" {
			emit(map[string]any{
				"type": "edit", "thread": thread,
				"id": id, "text": c.EditMessage.GetMessage().GetText(),
			})
		}
		return
	}

	ext := extractE2EE(e)
	if ext.skip || ext.text == "" {
		return
	}

	sender := e.Info.Sender.User
	if e.Info.IsFromMe {
		sender = selfFBID
	}
	tsMs := e.Info.Timestamp.UnixMilli()
	ts := float64(tsMs)

	name := ""
	if !isGroup && !e.Info.IsFromMe && e.Info.PushName != "" && e.Info.PushName != "username" {
		name = e.Info.PushName
	}
	if !isGroup {
		if fbid, err := strconv.ParseInt(e.Info.Chat.User, 10, 64); err == nil {
			go fetchContact(fbid)
			markE2EEKey(fbid)
		}
		go subscribePresence(e.Info.Chat.User)
	} else {
		if gid, err := strconv.ParseInt(e.Info.Chat.User, 10, 64); err == nil {
			markE2EEKey(gid)
		}
		go fetchE2EEGroupInfo(e.Info.Chat)
		if !e.Info.IsFromMe && sender != "" {
			if sid, err := strconv.ParseInt(sender, 10, 64); err == nil {
				go fetchContact(sid)
			}
		}
	}

	isLive := !e2eeOfflineSync.Load()
	if e.Info.IsFromMe {
		noteThreadWatermark(thread, tsMs+999, tsMs)
	} else {
		noteThreadWatermark(thread, 0, tsMs)
	}
	wm := getThreadWatermark(thread)
	unread := !e.Info.IsFromMe && !(wm.readUpTo > 0 && tsMs <= wm.readUpTo+999)

	thEv := map[string]any{
		"type": "thread", "id": thread, "name": name,
		"snippet": ext.text, "lastActivity": ts, "unread": unread, "e2ee": true,
	}
	if wm.readUpTo > 0 {
		thEv["readUpTo"] = wm.readUpTo
	}
	if isGroup {
		thEv["ttype"] = int64(table.ENCRYPTED_OVER_WA_GROUP)
	} else {
		thEv["ttype"] = int64(table.ENCRYPTED_OVER_WA_ONE_TO_ONE)
		thEv["contact"] = e.Info.Chat.User
	}
	emit(thEv)

	emitExtractedE2EE(thread, e.Info.ID, sender, tsMs, isLive, ext, nil)
}

// Backfill: the phone sends recent encrypted history when a device registers.
func handleHistorySync(e *events.HistorySync) {
	if e.Data == nil {
		return
	}
	for _, conv := range e.Data.GetConversations() {
		jid := conv.GetID()
		raw := strings.SplitN(jid, "@", 2)[0]
		if raw == "" {
			continue
		}
		thread := "e:" + raw
		isGroup := strings.Contains(jid, "@g.us")
		if isGroup {
			markE2EEGroup(raw)
		} else if fbid, err := strconv.ParseInt(raw, 10, 64); err == nil {
			markE2EEKey(fbid)
		}
		var name, lastText string
		var lastTs float64
		for _, hm := range conv.GetMessages() {
			wmi := hm.GetMessage()
			if wmi == nil {
				continue
			}
			text := msgText(wmi.GetMessage())
			if text == "" {
				continue
			}
			key := wmi.GetKey()
			sender := raw
			if isGroup {
				sender = strings.SplitN(key.GetParticipant(), "@", 2)[0]
			}
			if key.GetFromMe() {
				sender = selfFBID
			} else if !isGroup && wmi.GetPushName() != "" {
				name = wmi.GetPushName()
			}
			ts := float64(wmi.GetMessageTimestamp()) * 1000
			emit(map[string]any{
				"type": "message", "id": key.GetID(), "thread": thread, "sender": sender,
				"text": text, "ts": ts, "live": false, "system": false,
			})
			lastText, lastTs = text, ts
		}
		if lastText != "" {
			thEv := map[string]any{
				"type": "thread", "id": thread, "name": name,
				"snippet": lastText, "lastActivity": lastTs, "unread": false, "e2ee": true,
			}
			if isGroup {
				thEv["ttype"] = int64(table.ENCRYPTED_OVER_WA_GROUP)
			} else {
				thEv["ttype"] = int64(table.ENCRYPTED_OVER_WA_ONE_TO_ONE)
				thEv["contact"] = raw
			}
			emit(thEv)
		}
	}
}
