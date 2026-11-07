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
}
