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

// historyStatus tells the app whether history is already restored, needs a PIN, or is unavailable.
func historyStatus() {
	loadHistoryDevice()
	histMu.Lock()
	restored := histDevice != nil && histDevice.ID != ""
	histMu.Unlock()
	if restored {
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
	decoded.Info = types.MessageInfo{
		MessageSource: types.MessageSource{
			Chat:     types.NewJID(chatUser, types.MessengerServer),
			Sender:   types.NewJID(md.GetSenderID(), types.MessengerServer),
			IsFromMe: md.GetSenderID() == selfFBID,
		},
		ID:        md.GetMessageID(),
		Timestamp: time.UnixMilli(tsMs),
	}
	return &decoded, nil
}

// loadEncryptedHistory fetches one page (up to 50 messages) older than `before` (ms).
func loadEncryptedHistory(thread string, before int64) {
	loadHistoryDevice()
	histMu.Lock()
	device := histDevice
	histMu.Unlock()
	if device == nil || device.ID == "" {
		emitHistoryStatus("needPIN", "", nil)
		return
	}
	if client == nil {
		return
	}
	chatUser := strings.TrimPrefix(thread, "e:")
	if before <= 0 {
		before = time.Now().Add(time.Minute).UnixMilli()
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	page, err := client.GetSecureHistoryClient().FetchMessages(ctx, device, chatUser, true, before, 50)
	if err != nil {
		emit(map[string]any{"type": "error", "msg": "history: " + err.Error()})
		return
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
	count := 0
	for _, m := range page.Decrypt(ctx, device, chatUser, self, false) {
		if m == nil || m.Message == nil {
			continue
		}
		ev, err := decodeArchive(chatUser, m.Message.Message, m.Message.TimestampMS)
		if err != nil || ev == nil {
			continue
		}
		text := fbText(ev)
		if text == "" && len(m.Attachments) > 0 {
			text = "[Tệp đính kèm]"
		}
		if text == "" {
			continue
		}
		msg := map[string]any{
			"type": "message", "id": ev.Info.ID, "thread": thread, "sender": ev.Info.Sender.User,
			"text": text, "ts": float64(ev.Info.Timestamp.UnixMilli()), "live": false, "system": false,
		}
		if ev.FBApplication != nil {
			if q := ev.FBApplication.GetMetadata().GetQuotedMessage(); q != nil && q.GetStanzaID() != "" {
				msg["replyToId"] = q.GetStanzaID()
			}
		}
		emit(msg)
		count++

		// Archived pictures/videos: the CDN path comes from the archive attachments.
		if len(m.Attachments) > 0 {
			paths := make(map[string]string, len(m.Attachments))
			for _, a := range m.Attachments {
				if a.ObjectID != "" && a.URL != "" {
					paths[a.ObjectID] = a.URL
				}
			}
			if kind, dl := mediaDownloader(ev, paths); dl != nil {
				msgID, th := ev.Info.ID, thread
				histMediaSem <- struct{}{}
				go func() {
					defer func() { <-histMediaSem }()
					data, ext, err := dl()
					if err != nil || len(data) == 0 {
						log.Warn().Err(err).Str("kind", kind).Msg("archive media download failed")
						return
					}
					if p := saveMedia(msgID, ext, data); p != "" {
						emit(map[string]any{"type": "media", "id": msgID, "thread": th, "kind": kind, "path": p})
					}
				}()
			}
		}
	}
	hasMore := page.Range.HasMoreBefore != nil && *page.Range.HasMoreBefore
	emit(map[string]any{"type": "historyPage", "thread": thread, "before": page.Range.Before, "hasMore": hasMore, "count": count})
	log.Info().Str("thread", thread).Int("count", count).Bool("hasMore", hasMore).Msg("encrypted history page")
	_ = fmt.Sprint
}
