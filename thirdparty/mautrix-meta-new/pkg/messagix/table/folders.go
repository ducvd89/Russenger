package table

type LSUpsertFolder struct {
	ThreadKey                    int64  `index:"0" json:",omitempty"`
	ThreadName                   string `index:"1" json:",omitempty"`
	ThreadPictureURL             string `index:"2" json:",omitempty"`
	LastActivityTimestampMS      int64  `index:"3" json:",omitempty"`
	LastReadWatermarkTimestampMS int64  `index:"4" json:",omitempty"`
	// 5 might be parent thread last activity
	// 6 is unknown

	Unrecognized map[int]any `json:",omitempty"`
}

type LSUpsertFolderSeenTimestamp struct {
	ParentThreadKey            int64 `index:"0" json:",omitempty"`
	LastSeenRequestTimestampMs int64 `index:"1" json:",omitempty"`

	Unrecognized map[int]any `json:",omitempty"`
}

// LSPopulateCommunityFolderData carries a community's name and details (Relay addition:
// the field layout was worked out from observed data, indexes 5 and 6 are still unidentified).
type LSPopulateCommunityFolderData struct {
	ThreadKey    int64  `index:"0" json:",omitempty"`
	NegThreadKey int64  `index:"1" json:",omitempty"`
	Name         string `index:"2" json:",omitempty"`
	Description  string `index:"3" json:",omitempty"`
	MemberCount  int64  `index:"4" json:",omitempty"`
	Field5       string `index:"5" json:",omitempty"`
	Field6       string `index:"6" json:",omitempty"`

	Unrecognized map[int]any `json:",omitempty"`
}
