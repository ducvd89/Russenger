package main

// Encrypted (E2EE) chat history: restored from Messenger's "secure storage" backup
// using the user's 6-digit PIN, then fetched page by page. Adapted from
// mautrix-meta's connector/history*.go and login_history.go.
//
// Commands:  historyStatus | restoreHistory {pin} | loadEncryptedHistory {thread, before}
// Events:    historyStatus {state, error?, attemptsRemaining?} | historyPage {thread, before, hasMore, count}

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waArmadilloBackupMessage"
	waConsumerApplication "go.mau.fi/whatsmeow/proto/waConsumerApplication"
	"go.mau.fi/whatsmeow/proto/waMsgApplication"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	"google.golang.org/protobuf/proto"

	"go.mau.fi/mautrix-meta/pkg/messagix/securehistory"
)

// at most 4 archive media downloads at once
var histMediaSem = make(chan struct{}, 4)

var (
	histMu     sync.Mutex
	histDevice *securehistory.Device
	histBackup *securehistory.Backup
)

func historyFile() string {
	dir, _ := os.UserConfigDir()
	d := filepath.Join(dir, "Relay")
	_ = os.MkdirAll(d, 0o700)
	return filepath.Join(d, "history-device.json")
}

func loadHistoryDevice() {
	histMu.Lock()
	defer histMu.Unlock()
	if histDevice != nil {
		return
	}
	raw, err := os.ReadFile(historyFile())
	if err != nil {
		return
	}
	var d securehistory.Device
	if json.Unmarshal(raw, &d) == nil && d.ID != "" {
		histDevice = &d
	}
}

// saveHistoryDevice persists the enrollment (contains secret keys: user-only file).
func saveHistoryDevice(d *securehistory.Device) error {
	raw, err := json.Marshal(d)
	if err != nil {
		return err
	}
	return os.WriteFile(historyFile(), raw, 0o600)
}

func emitHistoryStatus(state, errMsg string, attempts *int) {
	ev := map[string]any{"type": "historyStatus", "state": state}
	if errMsg != "" {
		ev["error"] = errMsg
	}
	if attempts != nil {
		ev["attemptsRemaining"] = *attempts
	}
	emit(ev)
}

func hasHistoryDevice() bool {
	loadHistoryDevice()
	histMu.Lock()
	defer histMu.Unlock()
	return histDevice != nil && histDevice.ID != ""
}

// historyStatus tells the app whether history is already restored, needs a PIN, or is unavailable.
func historyStatus() {
	if hasHistoryDevice() {
		emitHistoryStatus("restored", "", nil)
		return
	}
	if client == nil {
		emitHistoryStatus("error", "not connected", nil)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	backup, err := client.GetSecureHistoryClient().GetBackup(ctx)
	if errors.Is(err, securehistory.ErrNoPINBackup) {
		emitHistoryStatus("none", "", nil)
		return
	} else if err != nil {
		emitHistoryStatus("error", err.Error(), nil)
		return
	}
	histMu.Lock()
	histBackup = backup
	histMu.Unlock()
	emitHistoryStatus("needPIN", "", nil)
}

func restoreHistory(pin string) {
	if len(pin) != 6 || strings.Trim(pin, "0123456789") != "" {
		emitHistoryStatus("needPIN", "PIN gồm đúng 6 chữ số", nil)
		return
	}
	if client == nil || e2eeClient == nil || e2eeClient.Store == nil {
		emitHistoryStatus("error", "kênh mã hóa chưa sẵn sàng, thử lại sau vài giây", nil)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	hc := client.GetSecureHistoryClient()

	histMu.Lock()
	backup := histBackup
	histMu.Unlock()
	var err error
	if backup == nil {
		if backup, err = hc.GetBackup(ctx); err != nil {
			emitHistoryStatus("error", err.Error(), nil)
			return
		}
	}
	fail := func(err error) {
		var rec *securehistory.RecoveryError
		if errors.As(err, &rec) && rec.AttemptsRemaining != nil {
			emitHistoryStatus("needPIN", "PIN không đúng", rec.AttemptsRemaining)
			return
		}
		log.Warn().Err(err).Msg("restore history failed")
		emitHistoryStatus("needPIN", "Không khôi phục được: "+err.Error(), nil)
	}
	recovery, err := hc.RecoverPIN(ctx, backup, []byte(pin))
	if err != nil {
		fail(err)
		return
	}
	self, _ := strconv.ParseInt(selfFBID, 10, 64)
	virtual, err := hc.GetVirtualDevice(ctx, backup, self, recovery.Code)
	if err != nil {
		fail(err)
		return
	}
	epochs, err := virtual.DeriveEpochs()
	if err != nil {
		fail(err)
		return
	}
	enrollment, err := virtual.PrepareDevice(backup.ID, hc.RegistrationID, e2eeClient.Store.IdentityKey, epochs)
	if err != nil {
		fail(err)
		return
	}
	device, err := hc.AddDevice(ctx, enrollment)
	if err != nil {
		fail(err)
		return
	}
	if err := saveHistoryDevice(device); err != nil {
		emitHistoryStatus("error", "không lưu được khóa lịch sử: "+err.Error(), nil)
		return
	}
	histMu.Lock()
	histDevice = device
	histMu.Unlock()
	emitHistoryStatus("restored", "", nil)
}

func decodeArchive(chatUser string, bm *waArmadilloBackupMessage.BackupMessage, tsMs int64) (*events.FBMessage, error) {
	var decoded events.FBMessage
	switch payload := bm.GetPayload().(type) {
	case *waArmadilloBackupMessage.BackupMessage_EncryptedTransportMessage:
		var app waMsgApplication.MessageApplication
		if err := proto.Unmarshal(payload.EncryptedTransportMessage, &app); err != nil {
			return nil, err
		}
		var err error
		if decoded, err = whatsmeow.DecodeFBApplication(&app); err != nil {
			return nil, err
		}
	case *waArmadilloBackupMessage.BackupMessage_EncryptedTransportEvent,
		*waArmadilloBackupMessage.BackupMessage_EncryptedTransportLocallyTransformedMessage:
		return nil, nil
	case *waArmadilloBackupMessage.BackupMessage_MiTransportAdminMessage:
	default:
		return nil, errors.New("archive message without payload")
	}
	md := bm.GetMetadata()
	if md.GetMessageID() == "" || md.GetSenderID() == "" {
		return nil, errors.New("archive message without metadata")
	}
	chat := e2eeChatJID(chatUser)
	decoded.Info = types.MessageInfo{
		MessageSource: types.MessageSource{
			Chat:     chat,
			Sender:   types.NewJID(md.GetSenderID(), types.MessengerServer),
			IsFromMe: md.GetSenderID() == selfFBID,
			IsGroup:  chat.Server == types.GroupServer,
		},
		ID:        md.GetMessageID(),
		Timestamp: time.UnixMilli(tsMs),
	}
	return &decoded, nil
}

func fetchEncryptedHistoryPage(thread string, before int64, countLimit int, emitPageEvent bool) (nextBefore int64, hasMore bool, count int) {
	loadHistoryDevice()
	histMu.Lock()
	device := histDevice
	histMu.Unlock()
	if device == nil || device.ID == "" {
		if emitPageEvent {
			emitHistoryStatus("needPIN", "", nil)
		}
		return 0, false, 0
	}
	if client == nil {
		return 0, false, 0
	}
	chatUser := strings.TrimPrefix(thread, "e:")
	if before <= 0 {
		before = time.Now().Add(time.Minute).UnixMilli()
	}
	if countLimit <= 0 {
		countLimit = 50
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	page, err := client.GetSecureHistoryClient().FetchMessages(ctx, device, chatUser, true, before, countLimit)
	if err != nil {
		if emitPageEvent {
			emit(map[string]any{"type": "error", "msg": "history: " + err.Error()})
		} else {
			log.Debug().Err(err).Str("thread", thread).Msg("refreshEncryptedHistory failed")
		}
		return 0, false, 0
	}
	if page.Epochs != nil && len(page.Epochs.Edges) > 0 {
		if updated, err := device.WithEpochs(page.Epochs); err == nil {
			if saveHistoryDevice(updated) == nil {
				histMu.Lock()
				histDevice = updated
				histMu.Unlock()
				device = updated
			}
		}
	}
	self, _ := strconv.ParseInt(selfFBID, 10, 64)
	isGroup := isE2EEGroup(chatUser)

	type archiveReaction struct {
		tsMs int64
		text string
	}

	for _, m := range page.Decrypt(ctx, device, chatUser, self, isGroup) {
		if m == nil || m.Message == nil {
			continue
		}
		ev, err := decodeArchive(chatUser, m.Message.Message, m.Message.TimestampMS)
		if err != nil || ev == nil {
			continue
		}

		// Apply archived edits and reactions stored in Supplementals.
		reactions := map[string]archiveReaction{}
		var latestEdit *waConsumerApplication.ConsumerApplication_EditMessage
		var editTs int64
		for _, sup := range m.Supplementals {
			if sup == nil || sup.Message == nil || sup.Message.GetMetadata() == nil || sup.Message.GetMetadata().TimestampMS == nil {
				continue
			}
			switch sup.Message.GetPayload().(type) {
			case *waArmadilloBackupMessage.BackupMessage_EncryptedTransportEvent,
				*waArmadilloBackupMessage.BackupMessage_EncryptedTransportLocallyTransformedMessage,
				*waArmadilloBackupMessage.BackupMessage_MiTransportAdminMessage:
				continue
			}
			supTs := sup.Message.GetMetadata().GetTimestampMS()
			supEv, err := decodeArchive(chatUser, sup.Message, supTs)
			if err != nil || supEv == nil {
				continue
			}
			c := supEv.GetConsumerApplication().GetPayload().GetContent()
			if rm := c.GetReactionMessage(); rm != nil {
				actor := supEv.Info.Sender.User
				prev, ok := reactions[actor]
				if !ok || supTs > prev.tsMs || (supTs == prev.tsMs && rm.GetText() == "") {
					reactions[actor] = archiveReaction{tsMs: supTs, text: rm.GetText()}
				}
			} else if ed := c.GetEditMessage(); ed != nil && ed.GetMessage() != nil && supEv.Info.Sender == ev.Info.Sender {
				if latestEdit == nil || supTs > editTs {
					latestEdit = ed
					editTs = supTs
				}
			}
		}
		if latestEdit != nil {
			ev.Message = &waConsumerApplication.ConsumerApplication{
				Payload: &waConsumerApplication.ConsumerApplication_Payload{
					Payload: &waConsumerApplication.ConsumerApplication_Payload_Content{
						Content: &waConsumerApplication.ConsumerApplication_Content{
							Content: &waConsumerApplication.ConsumerApplication_Content_MessageText{
								MessageText: latestEdit.GetMessage(),
							},
						},
					},
				},
			}
		}

		var paths map[string]string
		if len(m.Attachments) > 0 {
			paths = make(map[string]string, len(m.Attachments))
			for _, a := range m.Attachments {
				if a.ObjectID != "" && a.URL != "" {
					paths[a.ObjectID] = a.URL
				}
			}
		}

		ext := extractE2EE(ev, paths)
		if ext.skip {
			continue
		}
		if ext.text == "" && len(m.Attachments) > 0 {
			ext.text = "📎 Attachment"
		}
		if ext.text == "" {
			continue
		}

		emitExtractedE2EE(thread, ev.Info.ID, ev.Info.Sender.User, ev.Info.Timestamp.UnixMilli(), false, ext, histMediaSem)
		for actor, r := range reactions {
			if r.text != "" && actor != "" {
				emit(map[string]any{
					"type": "reaction", "thread": thread,
					"id": ev.Info.ID, "emoji": r.text, "actor": actor,
				})
			}
		}
		count++
	}

	hasMore = page.Range.HasMoreBefore != nil && *page.Range.HasMoreBefore
	nextBefore = page.Range.Before
	if emitPageEvent {
		// If we reached the beginning of E2EE Secure Storage and this chat has a pre-cutover
		// open thread, pull older plaintext history before closing out pagination.
		if !hasMore {
			if openID := getOpenThreadForE2EE(toInt64(chatUser)); openID != 0 {
				go loadEarlier(thread, "", nextBefore, false)
			}
		}
		emit(map[string]any{"type": "historyPage", "thread": thread, "before": nextBefore, "hasMore": hasMore, "count": count})
		log.Info().Str("thread", thread).Int("count", count).Bool("hasMore", hasMore).Msg("encrypted history page")
	}
	return nextBefore, hasMore, count
}

// loadEncryptedHistory fetches one page (up to 50 messages) older than `before` (ms).
func loadEncryptedHistory(thread string, before int64) {
	fetchEncryptedHistoryPage(thread, before, 50, true)
}

// refreshEncryptedHistory fetches the newest page(s) of Secure Storage when opening an
// E2EE conversation (or recovering an undecryptable message) so messages sent/received
// while this desktop device's offline queue expired still appear seamlessly.
func refreshEncryptedHistory(thread string, sinceTs int64) {
	nextBefore, hasMore, _ := fetchEncryptedHistoryPage(thread, 0, 35, false)
	if sinceTs > 0 {
		for page := 0; page < 3 && hasMore && nextBefore > sinceTs; page++ {
			prev := nextBefore
			nextBefore, hasMore, _ = fetchEncryptedHistoryPage(thread, nextBefore, 50, false)
			if nextBefore >= prev {
				break
			}
		}
	}
	_ = fmt.Sprint
}
