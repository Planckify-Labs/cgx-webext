//! The boundary between this crate and the embedding browser.
//!
//! The core never touches browser internals. Everything it needs from the browser (persistence, time, locale, UI-bound
//! operations, user prompts) goes through [`ExtensionHost`]; everything the browser needs to tell the extension goes
//! back out through [`ExtensionHost::dispatch_event`].

use crate::permissions::PermissionRequest;
use serde_json::{Value, json};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// No such API in the registry.
    UnknownApi,
    /// Known, but not available: not implemented yet, or not in this manifest version.
    NotAvailable,
    PermissionDenied,
    InvalidArguments,
    /// The call was valid but failed (quota exceeded, no such tab, ...). Becomes `runtime.lastError`/a rejection.
    Runtime,
    /// A bug: the registry claims the core handles this function but no handler exists.
    Unhandled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> ApiError {
        ApiError { kind, message: message.into() }
    }

    pub fn runtime(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorKind::Runtime, message)
    }

    pub fn invalid(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorKind::InvalidArguments, message)
    }

    pub fn to_json(&self) -> Value {
        json!({ "kind": format!("{:?}", self.kind), "message": self.message })
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult = Result<Value, ApiError>;
/// Completion callback. May be invoked synchronously or later, exactly once.
pub type Reply = Box<dyn FnOnce(ApiResult)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    /// Background page, event page or service worker.
    Background,
    /// Popup, options page, side panel, tab pages served from the extension origin.
    ExtensionPage,
    ContentScript,
    /// Anything else (e.g. a web page using `externally_connectable`).
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    pub context: ContextKind,
    /// The id the browser gave this context when it registered it (see `Dispatcher::context_created`). Required for
    /// messaging; `None` for callers that never registered.
    pub context_id: Option<u64>,
    pub tab_id: Option<i64>,
    pub frame_id: Option<i64>,
    pub url: Option<String>,
}

impl Caller {
    pub fn background() -> Caller {
        Caller { context: ContextKind::Background, context_id: None, tab_id: None, frame_id: None, url: None }
    }
}

/// One API invocation from an extension context. `function` is the fully-qualified name as written by the extension
/// minus the `chrome.`/`browser.` prefix, e.g. `storage.local.get`; `args` are the arguments without any callback.
#[derive(Debug, Clone)]
pub struct ApiCall {
    pub extension_id: String,
    pub function: String,
    pub args: Vec<Value>,
    pub caller: Caller,
}

impl ApiCall {
    pub fn arg(&self, i: usize) -> &Value {
        self.args.get(i).unwrap_or(&Value::Null)
    }
}

impl ContextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextKind::Background => "background",
            ContextKind::ExtensionPage => "extension_page",
            ContextKind::ContentScript => "content_script",
            ContextKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<ContextKind> {
        Some(match s {
            "background" => ContextKind::Background,
            "extension_page" => ContextKind::ExtensionPage,
            "content_script" => ContextKind::ContentScript,
            "other" => ContextKind::Other,
            _ => return None,
        })
    }
}

impl ApiCall {
    /// `{ "extensionId", "function", "args": [...], "caller": { "context", "tabId"?, "frameId"?, "url"? } }`
    pub fn to_json(&self) -> Value {
        let mut caller = json!({ "context": self.caller.context.as_str() });
        if let Some(c) = self.caller.context_id {
            caller["contextId"] = json!(c);
        }
        if let Some(t) = self.caller.tab_id {
            caller["tabId"] = json!(t);
        }
        if let Some(f) = self.caller.frame_id {
            caller["frameId"] = json!(f);
        }
        if let Some(u) = &self.caller.url {
            caller["url"] = json!(u);
        }
        json!({ "extensionId": self.extension_id, "function": self.function, "args": self.args, "caller": caller })
    }

    pub fn from_json(v: &Value) -> Result<ApiCall, String> {
        let str_field = |key: &str| v.get(key).and_then(Value::as_str).map(String::from).ok_or_else(|| format!("'{key}' must be a string"));
        let args = match v.get("args") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a.clone(),
            Some(_) => return Err("'args' must be an array".into()),
        };
        let caller = match v.get("caller") {
            None => Caller::background(),
            Some(c) => Caller {
                context: c
                    .get("context")
                    .and_then(Value::as_str)
                    .map_or(Some(ContextKind::Other), ContextKind::parse)
                    .ok_or("unknown caller context")?,
                context_id: c.get("contextId").and_then(Value::as_u64),
                tab_id: c.get("tabId").and_then(Value::as_i64),
                frame_id: c.get("frameId").and_then(Value::as_i64),
                url: c.get("url").and_then(Value::as_str).map(String::from),
            },
        };
        Ok(ApiCall { extension_id: str_field("extensionId")?, function: str_field("function")?, args, caller })
    }
}

pub trait ExtensionHost {
    /// Reads a persisted blob for an extension, e.g. `local`, `sync`, `permissions`.
    fn read_storage(&self, extension_id: &str, key: &str) -> Option<String>;
    fn write_storage(&self, extension_id: &str, key: &str, json: &str);

    fn now_ms(&self) -> i64;
    /// The browser UI locale as a BCP 47 tag, e.g. `en-US`.
    fn ui_locale(&self) -> String;

    /// Delivers an event to every context of the extension. `event` is the fully-qualified name, e.g. `alarms.onAlarm`;
    /// `args` are the listener arguments.
    fn dispatch_event(&self, extension_id: &str, event: &str, args: Vec<Value>);

    /// Carries an envelope to one context (see `Shim/webext-shim.js`, `api.handleHostMessage`). Envelopes to the same context
    /// must arrive in the order they were given. Delivering to a context that no longer exists is not an error.
    fn deliver_to_context(&self, context_id: u64, message: Value);

    /// The full `tabs.Tab` object for `MessageSender.tab`. Without it only `{ "id": tabId }` is provided.
    fn describe_tab(&self, _tab_id: i64) -> Option<Value> {
        None
    }

    /// Implements an API the core delegates (tabs, windows, action, cookies, ...). Must call `reply` exactly once.
    fn call_browser(&self, call: &ApiCall, reply: Reply);

    /// Asks the user to grant optional permissions. Must call `reply(true)` only if the user agreed.
    fn request_permissions(&self, extension_id: &str, request: &PermissionRequest, reply: Box<dyn FnOnce(bool)>);

    /// A hint that the earliest pending alarm is due at `at_ms`; the host should call `Dispatcher::tick` then.
    fn schedule_alarm_check(&self, _at_ms: i64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_call_json_roundtrip() {
        let call = ApiCall {
            extension_id: "id".into(),
            function: "tabs.query".into(),
            args: vec![json!({"active": true})],
            caller: Caller {
                context: ContextKind::ContentScript,
                context_id: Some(9),
                tab_id: Some(4),
                frame_id: Some(0),
                url: Some("https://a.com/".into()),
            },
        };
        let back = ApiCall::from_json(&call.to_json()).unwrap();
        assert_eq!((back.extension_id, back.function, back.args, back.caller), (call.extension_id, call.function, call.args, call.caller));
    }

    #[test]
    fn api_call_json_validation() {
        assert!(ApiCall::from_json(&json!({"function": "a.b"})).is_err());
        assert!(ApiCall::from_json(&json!({"extensionId": "x"})).is_err());
        assert!(ApiCall::from_json(&json!({"extensionId": "x", "function": "a", "args": 5})).is_err());
        assert!(ApiCall::from_json(&json!({"extensionId": "x", "function": "a", "caller": {"context": "weird"}})).is_err());
        let minimal = ApiCall::from_json(&json!({"extensionId": "x", "function": "a"})).unwrap();
        assert_eq!(minimal.caller, Caller::background());
        assert!(minimal.args.is_empty());
    }
}
