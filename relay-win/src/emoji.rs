//! Colour emoji. egui cannot draw colour fonts, so emoji are shown as Twemoji pictures
//! (tiny PNGs fetched from a CDN when first shown and held in memory only).
use unicode_segmentation::UnicodeSegmentation;

pub enum Seg<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

/// Characters below U+2BFF that show as emoji by default (no variation selector needed).
const EMOJI_PRESENTATION_BMP: &[u32] = &[
    0x231A, 0x231B, 0x23E9, 0x23EA, 0x23EB, 0x23EC, 0x23F0, 0x23F3, 0x25FD, 0x25FE, 0x2614, 0x2615, 0x267F, 0x2693, 0x26A1, 0x26AA, 0x26AB, 0x26BD, 0x26BE, 0x26C4,
    0x26C5, 0x26CE, 0x26D4, 0x26EA, 0x26F2, 0x26F3, 0x26F5, 0x26FA, 0x26FD, 0x2705, 0x270A, 0x270B, 0x2728, 0x274C, 0x274E, 0x2753, 0x2754, 0x2755, 0x2757, 0x2795,
    0x2796, 0x2797, 0x27B0, 0x27BF, 0x2B1B, 0x2B1C, 0x2B50, 0x2B55,
];

/// Is this grapheme cluster one emoji (including ZWJ sequences, skin tones, flags, keycaps)?
pub fn is_emoji(g: &str) -> bool {
    let Some(first) = g.chars().next() else { return false };
    let u = first as u32;
    // explicit emoji presentation (U+FE0F) or keycap (U+20E3) on a non-letter base
    if g.contains('\u{FE0F}') || g.contains('\u{20E3}') {
        return !first.is_alphabetic();
    }
    (0x1F000..=0x1FAFF).contains(&u) || EMOJI_PRESENTATION_BMP.contains(&u)
}

/// Split text into plain runs and emoji.
pub fn split(text: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, g) in text.grapheme_indices(true) {
        if is_emoji(g) {
            if let Some(s) = run_start.take() {
                out.push(Seg::Text(&text[s..i]));
            }
            out.push(Seg::Emoji(g));
        } else if run_start.is_none() {
            run_start = Some(i);
        }
    }
    if let Some(s) = run_start {
        out.push(Seg::Text(&text[s..]));
    }
    out
}

pub fn contains_emoji(text: &str) -> bool {
    text.graphemes(true).any(is_emoji)
}

/// Some(n) when the text is nothing but n emoji (and spaces).
pub fn only_emoji(text: &str) -> Option<usize> {
    let mut n = 0;
    for g in text.graphemes(true) {
        if g.trim().is_empty() {
            continue;
        }
        if !is_emoji(g) {
            return None;
        }
        n += 1;
    }
    (n > 0).then_some(n)
}

/// Twemoji picture address for one emoji: lower-case code points joined by '-', and the
/// variation selector dropped unless the sequence has a zero-width joiner.
pub fn url(emoji: &str) -> String {
    let has_zwj = emoji.contains('\u{200D}');
    let cps: Vec<String> = emoji.chars().filter(|c| has_zwj || *c != '\u{FE0F}').map(|c| format!("{:x}", c as u32)).collect();
    format!("https://cdn.jsdelivr.net/gh/jdecked/twemoji@15.1.0/assets/72x72/{}.png", cps.join("-"))
}

/// Address to draw an emoji from: a saved copy when there is one (works offline), otherwise the
/// CDN address while a copy is saved for next time.
pub fn uri(emoji: &str) -> eframe::egui::ImageSource<'static> {
    // the common ones are part of the program; anything else is fetched once and then kept
    let code = file_code(emoji);
    if let Some(bytes) = embedded::get(&code) {
        return eframe::egui::ImageSource::Bytes { uri: format!("bytes://emoji/{code}.png").into(), bytes: eframe::egui::load::Bytes::Static(bytes) };
    }
    eframe::egui::ImageSource::Uri(crate::cache::emoji_uri(&url(emoji)).into())
}

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/emoji_embedded.rs"));
}

fn file_code(emoji: &str) -> String {
    let has_zwj = emoji.contains('\u{200D}');
    emoji.chars().filter(|c| has_zwj || *c != '\u{FE0F}').map(|c| format!("{:x}", c as u32)).collect::<Vec<_>>().join("-")
}

/// Emoji offered by the composer's picker.
pub const PICKER: &str = "😀 😃 😄 😁 😆 😅 😂 🤣 😊 😇 🙂 🙃 😉 😌 😍 🥰 😘 😗 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🤐 😐 😑 😶 😏 😒 🙄 😬 😮‍💨 🤥 😌 😔 😪 🤤 😴 😷 🤒 🤕 🤢 🤮 🥵 🥶 🥴 😵 🤯 🤠 🥳 😎 🤓 🧐 😕 😟 🙁 ☹️ 😮 😯 😲 😳 🥺 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 👿 💀 ☠️ 💩 🤡 👻 👽 🤖 🙈 🙉 🙊 👍 👎 👌 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ ✋ 🤚 🖐️ 🖖 👋 🤝 🙏 💪 👏 🙌 👐 🤲 ❤️ 🧡 💛 💚 💙 💜 🖤 🤍 💔 ❣️ 💕 💞 💓 💗 💖 💘 💝 🔥 ✨ ⭐ 🌟 💥 💯 ✅ ❌ ❓ ❗ 🎉 🎊 🎁 🎂 🍺 ☕ 🍕 🍔 🍜 🌹 🌞 🌙 ⚡ 🌈 ☔ 🐶 🐱 🐷 🐸 🐵 🦁 🐔 🐧 🦄 🚗 ✈️ 🏠 💻 📱 🎮 🎧 📷 💰 ⏰ 🔔 💡 📌";

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> String {
        split(text).iter().map(|s| match s { Seg::Text(_) => 'T', Seg::Emoji(_) => 'E' }).collect()
    }

    #[test]
    fn splits_text_and_emoji() {
        assert_eq!(kinds("hi 😀 there"), "TET");
        assert_eq!(kinds("😀😂"), "EE");
        assert_eq!(kinds("no emoji, only text: ✓ 1 2 3"), "T");
        assert_eq!(kinds("ok 👍🏽!"), "TET"); // skin tone stays part of one emoji
        assert_eq!(kinds("👨‍👩‍👧 family"), "ET"); // ZWJ sequence is one emoji
        assert_eq!(kinds("🇻🇳"), "E"); // flag
        assert_eq!(kinds("❤️ and ❤"), "ET"); // plain U+2764 without VS16 is text, merged into the run
    }

    #[test]
    fn picture_addresses() {
        assert!(url("😀").ends_with("/1f600.png"));
        assert!(url("❤️").ends_with("/2764.png")); // VS16 dropped
        assert!(url("👨‍👩‍👧").ends_with("/1f468-200d-1f469-200d-1f467.png")); // kept with ZWJ
        assert!(url("🇻🇳").ends_with("/1f1fb-1f1f3.png"));
        assert!(url("1️⃣").ends_with("/31-20e3.png"));
    }

    #[test]
    fn emoji_only() {
        assert_eq!(only_emoji("😂 🔥"), Some(2));
        assert_eq!(only_emoji("😂 hi"), None);
        assert_eq!(only_emoji("   "), None);
    }

    #[test]
    fn picker_has_no_text_entries() {
        assert!(PICKER.split_whitespace().all(is_emoji), "every picker entry must be recognised as an emoji");
    }
}
