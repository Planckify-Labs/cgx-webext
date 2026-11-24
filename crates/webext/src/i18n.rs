//! `chrome.i18n` / `browser.i18n`: `_locales/*/messages.json` lookup, placeholders and substitutions.

use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
struct Message {
    text: String,
    placeholders: HashMap<String, String>, // lowercase name -> content
}

#[derive(Debug, Clone)]
pub struct I18n {
    extension_id: String,
    default_locale: String,
    ui_locale: String,
    locales: HashMap<String, HashMap<String, Message>>, // locale (underscore form) -> lowercase name -> message
}

fn normalize_locale(l: &str) -> String {
    l.replace('-', "_")
}

impl I18n {
    pub fn new(extension_id: &str, default_locale: &str, ui_locale: &str) -> I18n {
        I18n {
            extension_id: extension_id.into(),
            default_locale: normalize_locale(default_locale),
            ui_locale: normalize_locale(ui_locale),
            locales: HashMap::new(),
        }
    }

    pub fn ui_locale(&self) -> &str {
        &self.ui_locale
    }

    pub fn has_locale(&self, locale: &str) -> bool {
        self.locales.contains_key(&normalize_locale(locale))
    }

    /// Adds the contents of one `_locales/<locale>/messages.json`.
    pub fn add_locale(&mut self, locale: &str, messages_json: &str) -> Result<usize, String> {
        let text = crate::manifest::strip_json_comments(messages_json);
        let root: Value = serde_json::from_str(&text).map_err(|e| format!("{locale}: {e}"))?;
        let obj = root.as_object().ok_or_else(|| format!("{locale}: messages.json must be an object"))?;
        let mut map = HashMap::new();
        for (name, entry) in obj {
            let Some(message) = entry.get("message").and_then(Value::as_str) else { continue };
            let mut m = Message { text: message.to_string(), ..Default::default() };
            if let Some(ph) = entry.get("placeholders").and_then(Value::as_object) {
                for (pname, pv) in ph {
                    if let Some(content) = pv.get("content").and_then(Value::as_str) {
                        m.placeholders.insert(pname.to_lowercase(), content.to_string());
                    }
                }
            }
            map.insert(name.to_lowercase(), m);
        }
        let count = map.len();
        self.locales.insert(normalize_locale(locale), map);
        Ok(count)
    }

    /// Locales to consult, most specific first: `pt_BR`, `pt`, then the default locale.
    fn fallback_chain(&self) -> Vec<String> {
        let mut chain = vec![self.ui_locale.clone()];
        if let Some((lang, _)) = self.ui_locale.split_once('_') {
            chain.push(lang.to_string());
        }
        chain.push(self.default_locale.clone());
        chain
    }

    fn predefined(&self, name: &str) -> Option<String> {
        let rtl = ["ar", "fa", "he", "ur", "ps", "sd", "ug", "yi", "dv"].contains(&self.ui_locale.split('_').next().unwrap_or(""));
        Some(match name {
            "@@extension_id" => self.extension_id.clone(),
            "@@ui_locale" => self.ui_locale.clone(),
            "@@bidi_dir" => if rtl { "rtl" } else { "ltr" }.into(),
            "@@bidi_reversed_dir" => if rtl { "ltr" } else { "rtl" }.into(),
            "@@bidi_start_edge" => if rtl { "right" } else { "left" }.into(),
            "@@bidi_end_edge" => if rtl { "left" } else { "right" }.into(),
            _ => return None,
        })
    }

    /// `i18n.getMessage`. Returns an empty string for unknown messages, like the browsers do.
    pub fn get_message(&self, name: &str, substitutions: &[String]) -> String {
        if let Some(v) = self.predefined(name) {
            return v;
        }
        let key = name.to_lowercase();
        for locale in self.fallback_chain() {
            if let Some(m) = self.locales.get(&locale).and_then(|l| l.get(&key)) {
                return expand(&m.text, &m.placeholders, substitutions);
            }
        }
        String::new()
    }

    /// Replaces `__MSG_name__` tokens (as used in manifests and CSS) with their messages. Unknown names are left as is.
    pub fn localize(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("__MSG_") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 6..];
            let end = after.find("__");
            let name =
                end.map(|e| &after[..e]).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@'));
            match (name, end) {
                (Some(name), Some(e)) => {
                    let msg = self.get_message(name, &[]);
                    if msg.is_empty() && !self.has_message(name) {
                        out.push_str(&rest[start..start + 6 + e + 2]);
                    } else {
                        out.push_str(&msg);
                    }
                    rest = &after[e + 2..];
                }
                _ => {
                    out.push_str("__MSG_");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    pub fn has_message(&self, name: &str) -> bool {
        self.predefined(name).is_some() || {
            let key = name.to_lowercase();
            self.fallback_chain().iter().any(|l| self.locales.get(l).is_some_and(|m| m.contains_key(&key)))
        }
    }
}

/// Pure-Rust implementation of `i18n.detectLanguage(text)`: identifies script-distinct languages
/// (Japanese, Korean, Chinese, Russian/Ukrainian, Arabic/Persian, Hebrew, Greek, Hindi, Thai, etc.)
/// and major Latin-script languages using stop-word and character-n-gram cues.
pub fn detect_language(text: &str) -> Value {
    use serde_json::json;

    let mut script_counts: HashMap<&'static str, usize> = HashMap::new();
    let mut hiragana_katakana = 0usize;
    let mut han = 0usize;
    let mut cyrillic_uk = 0usize;
    let mut arabic_fa = 0usize;
    let mut latin_letters = 0usize;
    let mut total_letters = 0usize;

    for ch in text.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        total_letters += 1;
        let cp = ch as u32;
        match cp {
            0x3040..=0x30FF | 0x31F0..=0x31FF => {
                hiragana_katakana += 1;
                *script_counts.entry("ja").or_default() += 1;
            }
            0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => {
                *script_counts.entry("ko").or_default() += 1;
            }
            0x4E00..=0x9FFF | 0x3400..=0x4DBF => {
                han += 1;
            }
            0x0400..=0x04FF => {
                if matches!(ch, 'і' | 'І' | 'ї' | 'Ї' | 'є' | 'Є' | 'ґ' | 'Ґ') {
                    cyrillic_uk += 1;
                }
                *script_counts.entry("cyrillic").or_default() += 1;
            }
            0x0370..=0x03FF => *script_counts.entry("el").or_default() += 1,
            0x0590..=0x05FF => *script_counts.entry("he").or_default() += 1,
            0x0600..=0x06FF => {
                if matches!(ch, 'پ' | 'چ' | 'ژ' | 'گ' | 'ی' | 'ک') {
                    arabic_fa += 1;
                }
                *script_counts.entry("arabic").or_default() += 1;
            }
            0x0900..=0x097F => *script_counts.entry("hi").or_default() += 1,
            0x0980..=0x09FF => *script_counts.entry("bn").or_default() += 1,
            0x0B80..=0x0BFF => *script_counts.entry("ta").or_default() += 1,
            0x0E00..=0x0E7F => *script_counts.entry("th").or_default() += 1,
            0x0530..=0x058F => *script_counts.entry("hy").or_default() += 1,
            0x10A0..=0x10FF => *script_counts.entry("ka").or_default() += 1,
            _ if cp < 0x0250 || (0x1E00..=0x1EFF).contains(&cp) => {
                latin_letters += 1;
            }
            _ => {}
        }
    }

    if total_letters == 0 {
        return json!({ "isReliable": false, "languages": [] });
    }

    let mut lang_weights: HashMap<&'static str, usize> = HashMap::new();
    if han > 0 {
        if hiragana_katakana > 0 {
            *lang_weights.entry("ja").or_default() += han + hiragana_katakana;
        } else {
            *lang_weights.entry("zh").or_default() += han;
        }
    } else if hiragana_katakana > 0 {
        *lang_weights.entry("ja").or_default() += hiragana_katakana;
    }

    for (script, count) in &script_counts {
        match *script {
            "ja" => {}
            "cyrillic" => {
                let code = if cyrillic_uk > 0 { "uk" } else { "ru" };
                *lang_weights.entry(code).or_default() += *count;
            }
            "arabic" => {
                let code = if arabic_fa > 0 { "fa" } else { "ar" };
                *lang_weights.entry(code).or_default() += *count;
            }
            other => {
                *lang_weights.entry(other).or_default() += *count;
            }
        }
    }

    let mut latin_confident = false;
    if latin_letters > 0 {
        let (code, confident) = detect_latin_language(text);
        latin_confident = confident;
        *lang_weights.entry(code).or_default() += latin_letters;
    }

    let mut ranked: Vec<(&'static str, usize)> = lang_weights.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(3);

    let sum: usize = ranked.iter().map(|(_, c)| *c).sum::<usize>().max(1);
    let languages: Vec<Value> = ranked
        .iter()
        .map(|(code, count)| {
            let pct = ((*count as f64 / sum as f64) * 100.0).round() as u64;
            json!({ "language": code, "percentage": pct.clamp(1, 100) })
        })
        .collect();

    let top_share = ranked.first().map(|(_, c)| *c as f64 / total_letters as f64).unwrap_or(0.0);
    let non_latin_top = ranked.first().is_some_and(|(code, _)| {
        matches!(*code, "ja" | "ko" | "zh" | "ru" | "uk" | "el" | "he" | "ar" | "fa" | "hi" | "bn" | "ta" | "th" | "hy" | "ka")
    });
    let is_reliable = total_letters >= 10 && top_share >= 0.6 && (non_latin_top || latin_confident);

    json!({ "isReliable": is_reliable, "languages": languages })
}

fn detect_latin_language(text: &str) -> (&'static str, bool) {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).collect();

    // Language profiles: (code, distinctive diacritics, high-frequency stop words)
    const PROFILES: &[(&str, &str, &[&str])] = &[
        (
            "en",
            "",
            &[
                "the", "and", "that", "have", "for", "not", "with", "you", "this", "but", "from", "they", "which", "would", "there",
                "their", "what", "about", "hello", "world",
            ],
        ),
        (
            "es",
            "ñ¿¡áéíóú",
            &[
                "que", "los", "las", "por", "con", "para", "una", "como", "pero", "más", "este", "esta", "hola", "mundo", "del", "son",
                "está", "entre", "también",
            ],
        ),
        (
            "fr",
            "çœêîôûàèùé",
            &[
                "les", "des", "est", "que", "dans", "une", "pour", "pas", "sur", "plus", "avec", "tout", "mais", "nous", "vous", "bonjour",
                "monde", "cette", "sont",
            ],
        ),
        (
            "de",
            "ßäöü",
            &[
                "der", "die", "und", "den", "von", "zu", "das", "mit", "sich", "des", "auf", "für", "ist", "nicht", "ein", "eine", "auch",
                "hallo", "welt", "sind",
            ],
        ),
        (
            "pt",
            "ãõçáéíóúâêô",
            &[
                "que", "não", "uma", "com", "para", "por", "mais", "como", "mas", "foi", "dos", "das", "seu", "sua", "olá", "mundo",
                "está", "você", "são",
            ],
        ),
        (
            "it",
            "àèéìòù",
            &[
                "che", "non", "per", "una", "con", "sono", "della", "anche", "come", "questo", "questa", "ciao", "mondo", "degli", "nella",
                "tutto", "stato", "più",
            ],
        ),
        (
            "nl",
            "ĳëï",
            &[
                "het", "een", "van", "dat", "niet", "zijn", "voor", "met", "ook", "naar", "maar", "door", "over", "heeft", "worden",
                "deze", "wereld", "hallo",
            ],
        ),
        (
            "pl",
            "ąćęłńóśźż",
            &[
                "nie", "się", "jest", "jak", "ale", "tylko", "przez", "jego", "tego", "oraz", "być", "może", "które", "bardzo", "cześć",
                "świat",
            ],
        ),
        (
            "tr",
            "ğışçöü",
            &[
                "bir",
                "için",
                "olan",
                "daha",
                "olarak",
                "gibi",
                "kadar",
                "ancak",
                "sonra",
                "tarafından",
                "merhaba",
                "dünya",
                "ile",
                "çok",
                "değil",
            ],
        ),
        (
            "vi",
            "ăâđêôơưàảãáạằẳẵắặầẩẫấậèẻẽéẹềểễếệìỉĩíịòỏõóọồổỗốộờởỡớợùủũúụừửữứựỳỷỹýỵ",
            &["của", "không", "trong", "những", "người", "được", "một", "cho", "với", "này", "các", "như", "khi", "xin", "chào"],
        ),
        (
            "sv",
            "åäö",
            &[
                "och", "att", "det", "som", "för", "med", "inte", "till", "har", "den", "och", "kan", "från", "eller", "var", "hej",
                "världen",
            ],
        ),
        (
            "id",
            "",
            &[
                "yang", "dan", "dari", "untuk", "dengan", "tidak", "pada", "dalam", "adalah", "ini", "itu", "akan", "juga", "atau", "halo",
                "dunia",
            ],
        ),
    ];

    let mut scores: Vec<(&'static str, usize)> = PROFILES
        .iter()
        .map(|(code, diacritics, stop_words)| {
            let mut score = 0usize;
            for w in &words {
                if stop_words.contains(w) {
                    score += 4;
                }
            }
            if !diacritics.is_empty() {
                for ch in lower.chars() {
                    if diacritics.contains(ch) {
                        score += 2;
                    }
                }
            }
            (*code, score)
        })
        .collect();

    scores.sort_by_key(|b| std::cmp::Reverse(b.1));
    let best = scores[0];
    let second = scores[1];
    if best.1 == 0 {
        return ("en", false);
    }
    let confident = best.1 >= 6 && best.1 > second.1;
    (best.0, confident)
}

/// Expands `$name$` placeholders, `$1`..`$9` substitutions and `$$`.
fn expand(text: &str, placeholders: &HashMap<String, String>, subs: &[String]) -> String {
    fn go(text: &str, placeholders: &HashMap<String, String>, subs: &[String], depth: usize, out: &mut String) {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '$' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            match chars.get(i + 1) {
                Some('$') => {
                    out.push('$');
                    i += 2;
                }
                Some(d @ '1'..='9') => {
                    let idx = d.to_digit(10).unwrap() as usize - 1;
                    if let Some(s) = subs.get(idx) {
                        out.push_str(s);
                    }
                    i += 2;
                }
                _ => {
                    // $name$
                    if let Some(close) = chars[i + 1..].iter().position(|&c| c == '$') {
                        let name: String = chars[i + 1..i + 1 + close].iter().collect();
                        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@') {
                            if let Some(content) = placeholders.get(&name.to_lowercase())
                                && depth < 4
                            {
                                go(content, &HashMap::new(), subs, depth + 1, out);
                            }
                            i += close + 2;
                            continue;
                        }
                    }
                    out.push('$');
                    i += 1;
                }
            }
        }
    }
    let mut out = String::new();
    go(text, placeholders, subs, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i18n(ui: &str) -> I18n {
        let mut i = I18n::new("abcdefghijklmnopabcdefghijklmnop", "en", ui);
        i.add_locale("en", r#"{
            "hello": {"message": "Hello"},
            "Greeting": {"message": "Hi $USER$, you have $COUNT$ new $1", "placeholders": {"user": {"content": "$1"}, "count": {"content": "$2"}}},
            "price": {"message": "Cost: $$5 and $unknown$ done"},
            "only_en": {"message": "English only"}
        }"#).unwrap();
        i.add_locale("pt", r#"{"hello": {"message": "Olá"}}"#).unwrap();
        i.add_locale("pt_BR", r#"{"hello": {"message": "Oi"}}"#).unwrap();
        i
    }

    #[test]
    fn fallback_chain() {
        assert_eq!(i18n("pt-BR").get_message("hello", &[]), "Oi");
        assert_eq!(i18n("pt_PT").get_message("hello", &[]), "Olá");
        assert_eq!(i18n("de").get_message("hello", &[]), "Hello");
        assert_eq!(i18n("pt_BR").get_message("only_en", &[]), "English only");
        assert_eq!(i18n("en").get_message("missing", &[]), "");
    }

    #[test]
    fn placeholders_and_substitutions() {
        let i = i18n("en");
        let subs = ["Ann".to_string(), "3".to_string()];
        assert_eq!(i.get_message("greeting", &subs), "Hi Ann, you have 3 new Ann");
        assert_eq!(i.get_message("GREETING", &subs), "Hi Ann, you have 3 new Ann");
        assert_eq!(i.get_message("greeting", &[]), "Hi , you have  new ");
        assert_eq!(i.get_message("price", &[]), "Cost: $5 and  done");
    }

    #[test]
    fn predefined_messages() {
        let i = i18n("ar");
        assert_eq!(i.get_message("@@extension_id", &[]), "abcdefghijklmnopabcdefghijklmnop");
        assert_eq!(i.get_message("@@ui_locale", &[]), "ar");
        assert_eq!(i.get_message("@@bidi_dir", &[]), "rtl");
        assert_eq!(i.get_message("@@bidi_start_edge", &[]), "right");
        assert_eq!(i18n("en").get_message("@@bidi_dir", &[]), "ltr");
    }

    #[test]
    fn manifest_localization_tokens() {
        let i = i18n("pt_BR");
        assert_eq!(i.localize("__MSG_hello__ world"), "Oi world");
        assert_eq!(i.localize("__MSG_missing__ stays"), "__MSG_missing__ stays");
        assert_eq!(i.localize("a __MSG_ b __MSG_hello__"), "a __MSG_ b Oi");
        assert_eq!(i.localize("no tokens"), "no tokens");
        assert_eq!(i.localize("__MSG_hello____MSG_hello__"), "OiOi");
    }

    #[test]
    fn bad_messages_file() {
        let mut i = I18n::new("x", "en", "en");
        assert!(i.add_locale("en", "nope").is_err());
        assert!(i.add_locale("en", "[]").is_err());
        assert_eq!(i.add_locale("en", r#"{"a": {"description": "no message"}, "b": {"message": "ok"}}"#).unwrap(), 1);
    }

    #[test]
    fn detect_language_scripts_and_latin() {
        assert_eq!(detect_language("The quick brown fox jumps over the lazy dog and this is English")["languages"][0]["language"], "en");
        assert_eq!(detect_language("The quick brown fox jumps over the lazy dog and this is English")["isReliable"], true);
        assert_eq!(detect_language("Bonjour tout le monde, nous sommes très heureux avec cette journée")["languages"][0]["language"], "fr");
        assert_eq!(detect_language("Hola mundo, los días son más largos para todos con esta canción")["languages"][0]["language"], "es");
        assert_eq!(detect_language("Hallo Welt, der schnelle Fuchs springt über den großen Hund")["languages"][0]["language"], "de");
        assert_eq!(detect_language("こんにちは世界、今日はとても良い天気ですね")["languages"][0]["language"], "ja");
        assert_eq!(detect_language("你好世界，今天天气非常好，我们去公园散步吧")["languages"][0]["language"], "zh");
        assert_eq!(detect_language("안녕하세요 세계 오늘 날씨가 정말 좋습니다")["languages"][0]["language"], "ko");
        assert_eq!(detect_language("Привет мир, сегодня очень хорошая погода")["languages"][0]["language"], "ru");
        assert_eq!(detect_language("Привіт світ, сьогодні дуже гарна погода в Україні")["languages"][0]["language"], "uk");
        assert_eq!(detect_language("مرحبا بالعالم هذا نص باللغة العربية")["languages"][0]["language"], "ar");
        assert_eq!(detect_language("12345 !!!")["languages"].as_array().unwrap().len(), 0);
        assert_eq!(detect_language("12345 !!!")["isReliable"], false);
    }
}
