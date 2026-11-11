//! The network APIs: `declarativeNetRequest`, `webRequest` and `webNavigation`.
//!
//! The browser reports each request at up to nine points of its life with [`Dispatcher::network_event`] and waits for the
//! answer before it goes on. The core applies the `declarativeNetRequest` rules of every extension ([`crate::dnr`]), sends
//! the `webRequest` events to the listeners whose filter matches, waits for the blocking ones, and combines everything into
//! one instruction for the browser:
//!
//! | stage | request details the browser gives | what the answer may contain |
//! |---|---|---|
//! | `beforeRequest` | `requestId, url, method, type, tabId, frameId, parentFrameId, initiator?, documentUrl?, requestBody?` | `cancel`, `redirectUrl` |
//! | `beforeSendHeaders` | + `requestHeaders` | `cancel`, `requestHeaders` |
//! | `sendHeaders` | + `requestHeaders` | |
//! | `headersReceived` | + `statusCode, statusLine, responseHeaders, securityInfo?` | `cancel`, `redirectUrl`, `responseHeaders` |
//! | `authRequired` | + `scheme, realm, challenger: { host, port }, isProxy` | `cancel`, `authCredentials: { username, password }` |
//! | `beforeRedirect` | + `redirectUrl, statusCode, responseHeaders, ip?, fromCache` | |
//! | `responseStarted` | + `statusCode, responseHeaders, ip?, fromCache` | |
//! | `completed` | same | |
//! | `errorOccurred` | + `error` | |
//!
//! Headers are `[{ name, value }]`. `type` is one of the names in [`ResourceType`]. An empty answer (`{}`) means "go on".
//! The browser asks `host_event("networkInterest")` which stages anybody cares about, and is told when that changes with
//! the event `__network.changed`, so that it can skip the round trip for the stages nobody listens to.
//!
//! Navigations are reported with `host_event("navigation", …)`; see [`Dispatcher::navigation_event`].

use crate::dispatch::{Dispatcher, Extension};
use crate::dnr::{self, Access, ActionType, Decision, HeaderOp, HeaderRule, Request, ResourceType};
use crate::host::*;
use crate::match_pattern::{MatchDialect, MatchPatternSet};
use crate::registry::Member;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

pub fn handles(full_name: &str) -> bool {
    let ns = full_name.split('.').next().unwrap_or("");
    matches!(ns, "declarativeNetRequest" | "webNavigation") || full_name == "webRequest.getSecurityInfo"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    BeforeRequest,
    BeforeSendHeaders,
    SendHeaders,
    HeadersReceived,
    AuthRequired,
    BeforeRedirect,
    ResponseStarted,
    Completed,
    ErrorOccurred,
}

impl Stage {
    const ALL: [Stage; 9] = [
        Stage::BeforeRequest,
        Stage::BeforeSendHeaders,
        Stage::SendHeaders,
        Stage::HeadersReceived,
        Stage::AuthRequired,
        Stage::BeforeRedirect,
        Stage::ResponseStarted,
        Stage::Completed,
        Stage::ErrorOccurred,
    ];

    /// The name the browser uses.
    pub fn name(self) -> &'static str {
        match self {
            Stage::BeforeRequest => "beforeRequest",
            Stage::BeforeSendHeaders => "beforeSendHeaders",
            Stage::SendHeaders => "sendHeaders",
            Stage::HeadersReceived => "headersReceived",
            Stage::AuthRequired => "authRequired",
            Stage::BeforeRedirect => "beforeRedirect",
            Stage::ResponseStarted => "responseStarted",
            Stage::Completed => "completed",
            Stage::ErrorOccurred => "errorOccurred",
        }
    }

    pub fn event(self) -> &'static str {
        match self {
            Stage::BeforeRequest => "webRequest.onBeforeRequest",
            Stage::BeforeSendHeaders => "webRequest.onBeforeSendHeaders",
            Stage::SendHeaders => "webRequest.onSendHeaders",
            Stage::HeadersReceived => "webRequest.onHeadersReceived",
            Stage::AuthRequired => "webRequest.onAuthRequired",
            Stage::BeforeRedirect => "webRequest.onBeforeRedirect",
            Stage::ResponseStarted => "webRequest.onResponseStarted",
            Stage::Completed => "webRequest.onCompleted",
            Stage::ErrorOccurred => "webRequest.onErrorOccurred",
        }
    }

    fn parse(s: &str) -> Option<Stage> {
        Stage::ALL.into_iter().find(|x| x.name() == s || x.event() == s)
    }

    /// Stages whose listeners may answer with a decision.
    fn can_block(self) -> bool {
        matches!(self, Stage::BeforeRequest | Stage::BeforeSendHeaders | Stage::HeadersReceived | Stage::AuthRequired)
    }
}

/// One `webRequest.on…​.addListener(callback, filter, extraInfoSpec)`.
#[derive(Debug, Clone)]
struct Listener {
    extension_id: String,
    context_id: u64,
    /// Chosen by the context; names the callback there.
    listener_id: u64,
    stage: Stage,
    urls: MatchPatternSet,
    /// Patterns for extension files (`moz-extension://<id>/web_accessible_resources/*`), which match patterns cannot
    /// express; matched as globs.
    extension_urls: Vec<String>,
    types: Option<Vec<ResourceType>>,
    tab_id: Option<i64>,
    window_id: Option<i64>,
    blocking: bool,
    request_headers: bool,
    response_headers: bool,
    request_body: bool,
}

/// A request stage waiting for blocking listeners.
struct Pending {
    stage: Stage,
    details: Value,
    waiting: Vec<(u64, u64)>,
    /// `(install order of the extension, its answer)`
    answers: Vec<(u64, Value)>,
    header_rules: Vec<OwnedHeaderRule>,
    reply: Reply,
}

#[derive(Debug, Clone)]
struct OwnedHeaderRule {
    extension_id: String,
    seq: u64,
    rule: HeaderRule,
}

#[derive(Debug, Clone, Default)]
struct NavFrame {
    url: String,
    parent: i64,
    error: bool,
    document_id: Option<String>,
}

#[derive(Default)]
pub struct NetworkState {
    listeners: Vec<Listener>,
    pending: HashMap<u64, Pending>,
    next_pending: u64,
    /// `allowAllRequests` rules that matched a frame's document: `(tab, frame) -> [(extension, priority)]`.
    frame_allow: HashMap<(i64, i64), Vec<(String, i64)>>,
    frame_parent: HashMap<(i64, i64), i64>,
    /// `securityInfo` of requests in flight, for Firefox's `webRequest.getSecurityInfo`.
    security: HashMap<String, Value>,
    /// Frames per tab, from navigation events.
    frames: HashMap<i64, HashMap<i64, NavFrame>>,
}

fn headers_from_json(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| Some((h.get("name")?.as_str()?.to_string(), h.get("value").and_then(Value::as_str).unwrap_or("").to_string())))
        .collect()
}

fn headers_to_json(headers: &[(String, String)]) -> Value {
    Value::Array(headers.iter().map(|(n, v)| json!({ "name": n, "value": v })).collect())
}

fn is_extension_url(url: &str) -> bool {
    url.starts_with("chrome-extension://") || url.starts_with("moz-extension://")
}

/// Whether a request belongs to another extension, which hides it.
fn foreign(ext: &Extension, url: &str, initiator: Option<&str>) -> bool {
    let base = ext.base_url.trim_end_matches('/');
    let other = |u: &str| is_extension_url(u) && !u.starts_with(base);
    other(url) || initiator.is_some_and(other)
}

fn dnr_access(ext: &Extension, url: &str, initiator: Option<&str>, tab_id: i64) -> Access {
    let plain = ext.permissions.has_api("declarativeNetRequest");
    let with_host = ext.permissions.has_api("declarativeNetRequestWithHostAccess");
    if (!plain && !with_host) || foreign(ext, url, initiator) {
        return Access::None;
    }
    // An origin has no path; host permissions are patterns over URLs.
    let initiator_ok = initiator.is_none_or(|i| i == "null" || ext.permissions.has_host(&format!("{}/", i.trim_end_matches('/'))));
    let host_ok = ext.permissions.has_host_for_tab(tab_id, url) && initiator_ok;
    match (host_ok, plain) {
        (true, _) => Access::Full,
        (false, true) => Access::BlockOnly,
        (false, false) => Access::None,
    }
}

impl Dispatcher {
    fn network_changed(&self, id: &str) {
        self.host.dispatch_event(id, "__network.changed", vec![]);
    }

    /// Which stages the browser has to report, and whether any extension wants navigations (`webNavigation`):
    /// `{ any, stages: [...], requestBody, navigation }`.
    pub fn network_interest(&self) -> Value {
        let mut stages: Vec<Stage> = vec![];
        let mut add = |s: Stage| {
            if !stages.contains(&s) {
                stages.push(s);
            }
        };
        let net = self.network.borrow();
        let mut request_body = false;
        for l in &net.listeners {
            add(l.stage);
            request_body |= l.request_body;
        }
        for ext in self.extensions.borrow().values() {
            if ext.dnr.has_rules() {
                add(Stage::BeforeRequest);
            }
            if ext.dnr.has_request_header_rules() {
                add(Stage::BeforeSendHeaders);
            }
            if ext.dnr.has_response_rules() {
                add(Stage::HeadersReceived);
            }
        }
        let navigation = self.extensions.borrow().values().any(|e| e.permissions.has_api("webNavigation"));
        json!({
            "any": !stages.is_empty(),
            "stages": stages.iter().map(|s| s.name()).collect::<Vec<_>>(),
            "requestBody": request_body,
            "navigation": navigation,
        })
    }

    // ---- The request pipeline ---------------------------------------------------------------------------------------

    /// One stage of one request; see the module documentation. `reply` receives the instruction for the browser, at once
    /// unless a blocking `webRequest` listener has to answer first.
    pub fn network_event(&self, stage: &str, details: &Value, reply: Reply) {
        let Some(stage) = Stage::parse(stage) else {
            return reply(Err(ApiError::invalid(format!("unknown network stage '{stage}'"))));
        };
        let text = |k: &str| details.get(k).and_then(Value::as_str);
        let url = text("url").unwrap_or("");
        let resource_type = text("type").and_then(ResourceType::parse).unwrap_or(ResourceType::Other);
        let tab_id = details.get("tabId").and_then(Value::as_i64).unwrap_or(-1);
        let frame_id = details.get("frameId").and_then(Value::as_i64).unwrap_or(0);
        let parent_frame_id = details.get("parentFrameId").and_then(Value::as_i64).unwrap_or(-1);
        let initiator = text("initiator").filter(|i| !i.is_empty());
        let Some(mut request) = Request::new(url, text("method").unwrap_or("GET"), resource_type, initiator, tab_id) else {
            return reply(Ok(json!({})));
        };
        request.top_host = self.browser.borrow().tab(tab_id).and_then(|t| crate::match_pattern::split_url(&t.url)).map(|u| u.host);
        if let Some(id) = text("requestId") {
            let mut net = self.network.borrow_mut();
            match stage {
                Stage::HeadersReceived => {
                    if let Some(info) = details.get("securityInfo").filter(|v| v.is_object()) {
                        net.security.insert(id.to_string(), info.clone());
                    }
                }
                Stage::Completed | Stage::ErrorOccurred => {
                    net.security.remove(id);
                }
                _ => {}
            }
        }

        // The frame tree, for allowAllRequests: a navigation of the tab starts over; a subframe's document request tells
        // where the frame hangs.
        if stage == Stage::BeforeRequest {
            let mut net = self.network.borrow_mut();
            match resource_type {
                ResourceType::MainFrame => {
                    net.frame_allow.retain(|(t, _), _| *t != tab_id);
                    net.frame_parent.retain(|(t, _), _| *t != tab_id);
                }
                ResourceType::SubFrame => {
                    net.frame_allow.remove(&(tab_id, frame_id));
                    net.frame_parent.insert((tab_id, frame_id), parent_frame_id);
                }
                _ => {}
            }
        }

        // declarativeNetRequest. Rules act before the request is sent, and again once the response headers are there.
        let mut header_rules: Vec<OwnedHeaderRule> = vec![];
        if matches!(stage, Stage::BeforeRequest | Stage::BeforeSendHeaders | Stage::HeadersReceived) {
            if stage == Stage::HeadersReceived {
                request = request.with_response_headers(headers_from_json(details.get("responseHeaders")));
            }
            let (decision, rules) = self.evaluate_rules(&request, frame_id, parent_frame_id, stage == Stage::BeforeRequest);
            header_rules = rules;
            if stage != Stage::BeforeSendHeaders
                && let Some((extension_id, d)) = decision
            {
                self.rule_matched(
                    &extension_id,
                    d.rule_id,
                    &d.ruleset_id,
                    details,
                    d.action != ActionType::Allow && d.action != ActionType::AllowAllRequests,
                );
                match d.action {
                    ActionType::Block => return reply(Ok(json!({ "cancel": true }))),
                    ActionType::Redirect | ActionType::UpgradeScheme => {
                        return reply(Ok(json!({ "redirectUrl": d.redirect_url })));
                    }
                    _ => {}
                }
            }
        }

        // webRequest listeners.
        let listeners = self.matching_listeners(stage, &request, details);
        let mut waiting = vec![];
        let pending_id = {
            let mut net = self.network.borrow_mut();
            net.next_pending += 1;
            net.next_pending
        };
        let mut deliveries = vec![];
        for (listener, listener_details) in listeners {
            let blocking = listener.blocking && stage.can_block();
            if blocking {
                waiting.push((listener.context_id, listener.listener_id));
            }
            deliveries.push((
                listener.context_id,
                json!({
                    "type": "webRequest",
                    "event": stage.event(),
                    "listenerId": listener.listener_id,
                    "replyTo": if blocking { json!(pending_id) } else { Value::Null },
                    "details": listener_details,
                }),
            ));
        }
        let pending = Pending { stage, details: details.clone(), waiting, answers: vec![], header_rules, reply };
        if pending.waiting.is_empty() {
            for (context, envelope) in deliveries {
                self.host.deliver_to_context(context, envelope);
            }
            return self.finish_stage(pending);
        }
        self.network.borrow_mut().pending.insert(pending_id, pending);
        for (context, envelope) in deliveries {
            self.host.deliver_to_context(context, envelope);
        }
    }

    /// Asks every extension's rules. Returns the action that wins and the header rules to apply, most recently installed
    /// extension first.
    fn evaluate_rules(
        &self,
        request: &Request,
        frame_id: i64,
        parent_frame_id: i64,
        remember_frame_allow: bool,
    ) -> (Option<(String, Decision)>, Vec<OwnedHeaderRule>) {
        let exts = self.extensions.borrow();
        let mut ordered: Vec<&Extension> = exts.values().filter(|e| e.dnr.has_rules()).collect();
        if ordered.is_empty() {
            return (None, vec![]);
        }
        ordered.sort_by_key(|e| std::cmp::Reverse(e.seq));
        let initiator = request.initiator_host.as_ref().map(|h| format!("https://{h}"));
        let mut winner: Option<(String, Decision)> = None;
        let mut header_rules = vec![];
        let mut net = self.network.borrow_mut();
        for ext in ordered {
            let access = dnr_access(ext, &request.url, initiator.as_deref(), request.tab_id);
            if access == Access::None {
                continue;
            }
            // A frame's own document request is judged on its own; everything inside inherits from the frame and its
            // ancestors.
            let mut frame_allow: Option<i64> = None;
            let mut frame = if request.resource_type.is_frame() { parent_frame_id } else { frame_id };
            let mut hops = 0;
            while frame >= 0 && hops < 32 {
                if let Some(list) = net.frame_allow.get(&(request.tab_id, frame)) {
                    for (id, priority) in list {
                        if *id == ext.id {
                            frame_allow = Some(frame_allow.map_or(*priority, |p: i64| p.max(*priority)));
                        }
                    }
                }
                frame = net.frame_parent.get(&(request.tab_id, frame)).copied().unwrap_or(-1);
                hops += 1;
            }
            let evaluation = ext.dnr.evaluate(request, access, frame_allow, &ext.base_url);
            for rule in evaluation.header_rules {
                header_rules.push(OwnedHeaderRule { extension_id: ext.id.clone(), seq: ext.seq, rule });
            }
            let Some(decision) = evaluation.decision else { continue };
            if remember_frame_allow && decision.action == ActionType::AllowAllRequests && request.resource_type.is_frame() {
                let target = if request.resource_type == ResourceType::MainFrame { 0 } else { frame_id };
                net.frame_allow.entry((request.tab_id, target)).or_default().push((ext.id.clone(), decision.priority));
            }
            // Across extensions: block, then redirect/upgrade, then allow; the most recently installed extension wins
            // among equals, and the list is already in that order.
            let class = |a: ActionType| match a {
                ActionType::Block => 0,
                ActionType::Redirect | ActionType::UpgradeScheme => 1,
                _ => 2,
            };
            if winner.as_ref().is_none_or(|(_, w)| class(decision.action) < class(w.action)) {
                winner = Some((ext.id.clone(), decision));
            }
        }
        if winner.as_ref().is_some_and(|(_, d)| class_blocks(d.action)) {
            header_rules.clear();
        }
        (winner, header_rules)
    }

    /// Bookkeeping for a rule that acted: `getMatchedRules`, the badge count and `onRuleMatchedDebug`.
    fn rule_matched(&self, extension_id: &str, rule_id: i64, ruleset_id: &str, details: &Value, counts: bool) {
        let now = self.host.now_ms();
        let tab_id = details.get("tabId").and_then(Value::as_i64).unwrap_or(-1);
        let (debug, badge) = {
            let mut exts = self.extensions.borrow_mut();
            let Some(ext) = exts.get_mut(extension_id) else { return };
            ext.dnr.record_match(rule_id, ruleset_id, tab_id, now);
            let badge = counts && ext.dnr.show_action_count && tab_id >= 0;
            if badge {
                *ext.dnr.action_counts.entry(tab_id).or_default() += 1;
            }
            (ext.permissions.has_api("declarativeNetRequestFeedback") && !crate::browser_api::is_firefox(ext), badge)
        };
        if badge {
            self.host.dispatch_event(extension_id, "__ui.changed", vec![json!("action")]);
        }
        if debug {
            let mut request = Map::new();
            for key in ["requestId", "url", "method", "frameId", "parentFrameId", "tabId", "type", "initiator", "documentId"] {
                if let Some(v) = details.get(key).filter(|v| !v.is_null()) {
                    request.insert(key.into(), v.clone());
                }
            }
            self.host.dispatch_event(
                extension_id,
                "declarativeNetRequest.onRuleMatchedDebug",
                vec![json!({ "request": request, "rule": { "ruleId": rule_id, "rulesetId": ruleset_id } })],
            );
        }
    }

    /// The listeners of a stage that may see this request, each with the details object for its extension and options.
    fn matching_listeners(&self, stage: Stage, request: &Request, details: &Value) -> Vec<(Listener, Value)> {
        let net = self.network.borrow();
        if !net.listeners.iter().any(|l| l.stage == stage) {
            return vec![];
        }
        let exts = self.extensions.borrow();
        let window_id = self.browser.borrow().tab(request.tab_id).map(|t| t.window_id);
        let initiator = details.get("initiator").and_then(Value::as_str);
        let parsed = crate::match_pattern::split_url(&request.url);
        let mut out = vec![];
        for l in net.listeners.iter().filter(|l| l.stage == stage) {
            let Some(ext) = exts.get(&l.extension_id) else { continue };
            if foreign(ext, &request.url, initiator) || !ext.permissions.has_host_for_tab(request.tab_id, &request.url) {
                continue;
            }
            if !parsed.as_ref().is_some_and(|u| l.urls.matches_parsed(u))
                && !l.extension_urls.iter().any(|p| crate::match_pattern::glob_match(p, &request.url, false))
            {
                continue;
            }
            if l.types.as_ref().is_some_and(|t| !t.contains(&request.resource_type)) {
                continue;
            }
            if l.tab_id.is_some_and(|t| t != request.tab_id) || l.window_id.is_some_and(|w| Some(w) != window_id) {
                continue;
            }
            out.push((l.clone(), listener_details(stage, details, request, l, crate::browser_api::is_firefox(ext))));
        }
        out
    }

    /// All blocking listeners answered (or there were none): works out the instruction for the browser.
    fn finish_stage(&self, mut pending: Pending) {
        let details = &pending.details;
        // The most recently installed extension's answer wins where two disagree.
        pending.answers.sort_by_key(|a| std::cmp::Reverse(a.0));
        let answers: Vec<&Value> = pending.answers.iter().map(|(_, v)| v).filter(|v| v.is_object()).collect();
        let mut out = Map::new();
        if answers.iter().any(|a| a.get("cancel").and_then(Value::as_bool) == Some(true)) {
            return (pending.reply)(Ok(json!({ "cancel": true })));
        }
        let first = |key: &str| answers.iter().find_map(|a| a.get(key).filter(|v| !v.is_null()));
        match pending.stage {
            Stage::BeforeRequest | Stage::HeadersReceived => {
                if let Some(url) = first("redirectUrl").and_then(Value::as_str) {
                    return (pending.reply)(Ok(json!({ "redirectUrl": url })));
                }
                // Firefox: { upgradeToSecure: true }
                if pending.stage == Stage::BeforeRequest
                    && answers.iter().any(|a| a.get("upgradeToSecure").and_then(Value::as_bool) == Some(true))
                    && let Some(rest) = details.get("url").and_then(Value::as_str).and_then(|u| u.strip_prefix("http:"))
                {
                    return (pending.reply)(Ok(json!({ "redirectUrl": format!("https:{rest}") })));
                }
            }
            Stage::AuthRequired => {
                if let Some(c) = first("authCredentials").filter(|c| c.is_object()) {
                    out.insert("authCredentials".into(), c.clone());
                }
            }
            _ => {}
        }
        let (key, request_side) = match pending.stage {
            Stage::BeforeSendHeaders => ("requestHeaders", true),
            Stage::HeadersReceived => ("responseHeaders", false),
            _ => ("", false),
        };
        if !key.is_empty() {
            let original = headers_from_json(details.get(key));
            let mut headers = match first(key) {
                Some(h) => headers_from_json(Some(h)),
                None => original.clone(),
            };
            // Header rules: the extension installed last goes first, and within it the highest priority.
            pending.header_rules.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.rule.priority.cmp(&a.rule.priority)));
            let ops: Vec<(usize, usize, &HeaderOp)> = pending
                .header_rules
                .iter()
                .enumerate()
                .flat_map(|(i, r)| {
                    let list = if request_side { &r.rule.request_headers } else { &r.rule.response_headers };
                    list.iter().map(move |op| (r.seq as usize, i, op))
                })
                .collect();
            let mut acted: Vec<usize> = vec![];
            dnr::apply_header_ops(&mut headers, ops.into_iter(), request_side, |rule| {
                if !acted.contains(&rule) {
                    acted.push(rule);
                }
            });
            for i in acted {
                let r = &pending.header_rules[i];
                self.rule_matched(&r.extension_id, r.rule.rule_id, &r.rule.ruleset_id, details, true);
            }
            if headers != original {
                out.insert(key.into(), headers_to_json(&headers));
            }
        }
        (pending.reply)(Ok(Value::Object(out)));
    }

    /// `__webRequest.*`: the shim registering listeners and answering blocking events.
    pub(crate) fn web_request_internal(&self, call: &ApiCall, reply: Reply) {
        let ctx = match self.calling_context(call) {
            Ok(c) => c,
            Err(e) => return reply(Err(e)),
        };
        let result = match call.function.as_str() {
            "__webRequest.addListener" => self.web_request_add_listener(call, ctx.id),
            "__webRequest.removeListener" => {
                let stage = call.arg(0).as_str().and_then(Stage::parse);
                let id = call.arg(1).as_u64();
                self.network
                    .borrow_mut()
                    .listeners
                    .retain(|l| !(l.context_id == ctx.id && Some(l.stage) == stage && Some(l.listener_id) == id));
                self.network_changed(&call.extension_id);
                Ok(Value::Null)
            }
            "__webRequest.reply" => {
                let (Some(pending_id), Some(listener_id)) = (call.arg(0).as_u64(), call.arg(1).as_u64()) else {
                    return reply(Err(ApiError::invalid("__webRequest.reply needs a request and a listener id")));
                };
                self.web_request_answer(pending_id, ctx.id, listener_id, call.arg(2).clone());
                Ok(Value::Null)
            }
            other => Err(ApiError::new(ErrorKind::UnknownApi, format!("'{other}' is not a known internal call"))),
        };
        reply(result);
    }

    fn web_request_add_listener(&self, call: &ApiCall, context_id: u64) -> ApiResult {
        let stage = call.arg(0).as_str().and_then(Stage::parse).ok_or_else(|| ApiError::invalid("unknown webRequest event"))?;
        let listener_id = call.arg(1).as_u64().ok_or_else(|| ApiError::invalid("listener id must be a number"))?;
        let filter = call.arg(2);
        let specs: Vec<&str> = call.arg(3).as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let blocking = specs.contains(&"blocking") || specs.contains(&"asyncBlocking");
        self.with_ext(call, |ext| {
            if !ext.permissions.has_api("webRequest") {
                return Err(ApiError::new(ErrorKind::PermissionDenied, "'webRequest' requires the \"webRequest\" permission"));
            }
            if blocking {
                let firefox = crate::browser_api::is_firefox(ext);
                let auth = stage == Stage::AuthRequired && ext.permissions.has_api("webRequestAuthProvider");
                if !ext.permissions.has_api("webRequestBlocking") && !auth {
                    return Err(ApiError::runtime(
                        "You do not have permission to use blocking webRequest listeners. Be sure to declare the webRequestBlocking permission in your manifest.",
                    ));
                }
                // Chrome keeps blocking listeners for Manifest V2 (and for onAuthRequired in V3).
                if !firefox && ext.manifest.manifest_version >= 3 && !auth {
                    return Err(ApiError::runtime(
                        "You do not have permission to use blocking webRequest listeners. Be sure to declare the webRequestBlocking permission in your manifest. Note that webRequestBlocking is only allowed for extensions that are installed using ExtensionInstallForcelist.",
                    ));
                }
            }
            Ok(())
        })?;
        let patterns: Vec<&str> = filter
            .get("urls")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::invalid("The filter must have a 'urls' list"))?
            .iter()
            .map(|v| v.as_str().ok_or_else(|| ApiError::invalid("'urls' must contain strings")))
            .collect::<Result<_, _>>()?;
        let mut urls = MatchPatternSet::new();
        let mut extension_urls = vec![];
        for p in &patterns {
            if is_extension_url(p) {
                extension_urls.push(p.to_string());
                continue;
            }
            urls.add(p, MatchDialect::Firefox).map_err(|_| ApiError::invalid(format!("'{p}' is not a valid URL pattern.")))?;
        }
        // Names this browser does not know are skipped, as Firefox does, so that lists written for another browser work.
        let types = filter
            .get("types")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|v| v.as_str().and_then(ResourceType::parse)).collect());
        let listener = Listener {
            extension_id: call.extension_id.clone(),
            context_id,
            listener_id,
            stage,
            urls,
            extension_urls,
            types,
            tab_id: filter.get("tabId").and_then(Value::as_i64),
            window_id: filter.get("windowId").and_then(Value::as_i64),
            blocking,
            request_headers: specs.contains(&"requestHeaders"),
            response_headers: specs.contains(&"responseHeaders"),
            request_body: specs.contains(&"requestBody"),
        };
        self.network.borrow_mut().listeners.push(listener);
        self.network_changed(&call.extension_id);
        Ok(Value::Null)
    }

    fn web_request_answer(&self, pending_id: u64, context_id: u64, listener_id: u64, answer: Value) {
        let done = {
            let mut net = self.network.borrow_mut();
            let Some(p) = net.pending.get_mut(&pending_id) else { return };
            let Some(i) = p.waiting.iter().position(|w| *w == (context_id, listener_id)) else { return };
            p.waiting.remove(i);
            let seq = self
                .bus
                .borrow()
                .context(context_id)
                .and_then(|c| self.extensions.borrow().get(&c.extension_id).map(|e| e.seq))
                .unwrap_or(0);
            p.answers.push((seq, answer));
            if p.waiting.is_empty() { net.pending.remove(&pending_id) } else { None }
        };
        if let Some(p) = done {
            self.finish_stage(p);
        }
    }

    /// A context went away: its listeners go, and requests waiting for it go on.
    pub(crate) fn network_context_destroyed(&self, context_id: u64) {
        let (changed, finished) = {
            let mut net = self.network.borrow_mut();
            let before = net.listeners.len();
            let owners: Vec<String> = net.listeners.iter().filter(|l| l.context_id == context_id).map(|l| l.extension_id.clone()).collect();
            net.listeners.retain(|l| l.context_id != context_id);
            let mut ready = vec![];
            for (id, p) in net.pending.iter_mut() {
                p.waiting.retain(|(c, _)| *c != context_id);
                if p.waiting.is_empty() {
                    ready.push(*id);
                }
            }
            let finished: Vec<Pending> = ready.into_iter().filter_map(|id| net.pending.remove(&id)).collect();
            (if net.listeners.len() != before { owners.into_iter().next() } else { None }, finished)
        };
        for p in finished {
            self.finish_stage(p);
        }
        if let Some(id) = changed {
            self.network_changed(&id);
        }
    }

    pub(crate) fn network_extension_removed(&self, extension_id: &str) {
        let mut net = self.network.borrow_mut();
        net.listeners.retain(|l| l.extension_id != extension_id);
        for list in net.frame_allow.values_mut() {
            list.retain(|(id, _)| id != extension_id);
        }
    }

    pub(crate) fn network_tab_removed(&self, tab_id: i64) {
        let mut net = self.network.borrow_mut();
        net.frame_allow.retain(|(t, _), _| *t != tab_id);
        net.frame_parent.retain(|(t, _), _| *t != tab_id);
        net.frames.remove(&tab_id);
    }

    // ---- declarativeNetRequest and webNavigation functions ------------------------------------------------------------

    pub(crate) fn network_api(&self, member: &Member, call: &ApiCall, reply: Reply) {
        if member.namespace == "webNavigation" {
            return self.web_navigation(member, call, reply);
        }
        let result = if member.namespace == "webRequest" {
            // Firefox: the TLS state of a request, known once its headers arrived.
            let id = call.arg(0).as_str().unwrap_or("");
            Ok(self.network.borrow().security.get(id).cloned().unwrap_or_else(|| json!({ "state": "insecure" })))
        } else {
            self.declarative_net_request(member, call)
        };
        reply(result);
    }

    fn save_rules(&self, id: &str) {
        let json = self.extensions.borrow().get(id).map(|e| e.dnr.to_json().to_string());
        if let Some(json) = json {
            self.host.write_storage(id, "dnr", &json);
        }
        self.network_changed(id);
    }

    fn declarative_net_request(&self, member: &Member, call: &ApiCall) -> ApiResult {
        let id = call.extension_id.as_str();
        let options = call.arg(0);
        let selected = |rules: &dnr::RuleSet| -> ApiResult {
            let ids: Option<Vec<i64>> =
                options.get("ruleIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect());
            Ok(Value::Array(
                rules.rules().iter().filter(|r| ids.as_ref().is_none_or(|ids| ids.contains(&r.id))).map(|r| r.source.clone()).collect(),
            ))
        };
        match member.name.as_str() {
            "updateDynamicRules" | "updateSessionRules" => {
                let session = member.name == "updateSessionRules";
                self.with_ext_mut(call, |e| e.dnr.update_rules(session, options).map_err(ApiError::runtime))?;
                self.save_rules(id);
                Ok(Value::Null)
            }
            "getDynamicRules" => self.with_ext(call, |e| selected(&e.dnr.dynamic)),
            "getSessionRules" => self.with_ext(call, |e| selected(&e.dnr.session)),
            "getEnabledRulesets" => self.with_ext(call, |e| Ok(json!(e.dnr.enabled_ruleset_ids()))),
            "updateEnabledRulesets" => {
                let pool = self.static_rule_pool_left(id);
                self.with_ext_mut(call, |e| {
                    let files = &e.files;
                    e.dnr.update_enabled_rulesets(options, pool, |p| files.text(p)).map_err(ApiError::runtime)
                })?;
                self.save_rules(id);
                Ok(Value::Null)
            }
            "getAvailableStaticRuleCount" => {
                let pool = self.static_rule_pool_left(id);
                self.with_ext(call, |e| {
                    Ok(json!((dnr::GUARANTEED_MINIMUM_STATIC_RULES + pool).saturating_sub(e.dnr.enabled_static_rule_count())))
                })
            }
            "updateStaticRules" => {
                self.with_ext_mut(call, |e| e.dnr.update_static_rules(options).map_err(ApiError::runtime))?;
                self.save_rules(id);
                Ok(Value::Null)
            }
            "getDisabledRuleIds" => self.with_ext(call, |e| {
                let ruleset =
                    options.get("rulesetId").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("rulesetId must be a string"))?;
                let set = e
                    .dnr
                    .static_rulesets
                    .iter()
                    .find(|s| s.id == ruleset)
                    .ok_or_else(|| ApiError::runtime(format!("Invalid ruleset id: {ruleset}.")))?;
                let mut ids: Vec<i64> = set.disabled_rule_ids.iter().copied().collect();
                ids.sort_unstable();
                Ok(json!(ids))
            }),
            "isRegexSupported" => {
                let regex = options.get("regex").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("regex must be a string"))?;
                let case_sensitive = options.get("isCaseSensitive").and_then(Value::as_bool).unwrap_or(false);
                Ok(match dnr::compile_regex(regex, case_sensitive) {
                    Ok(_) => json!({ "isSupported": true }),
                    Err(dnr::RegexProblem::SyntaxError) => json!({ "isSupported": false, "reason": "syntaxError" }),
                    Err(dnr::RegexProblem::MemoryLimitExceeded) => json!({ "isSupported": false, "reason": "memoryLimitExceeded" }),
                })
            }
            "testMatchOutcome" => {
                let url = options.get("url").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("url must be a string"))?;
                let resource_type = options
                    .get("type")
                    .and_then(Value::as_str)
                    .and_then(ResourceType::parse)
                    .ok_or_else(|| ApiError::invalid("type must be a resource type"))?;
                let initiator = options.get("initiator").and_then(Value::as_str);
                let tab_id = options.get("tabId").and_then(Value::as_i64).unwrap_or(-1);
                let method = options.get("method").and_then(Value::as_str).unwrap_or("get");
                let mut request =
                    Request::new(url, method, resource_type, initiator, tab_id).ok_or_else(|| ApiError::runtime("Invalid url"))?;
                if let Some(headers) = options.get("responseHeaders").and_then(Value::as_object) {
                    let list = headers
                        .iter()
                        .flat_map(|(name, values)| {
                            values
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(|v| (name.clone(), v.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    request = request.with_response_headers(list);
                }
                self.with_ext(call, |e| {
                    let access = dnr_access(e, url, initiator, tab_id);
                    let outcome = e.dnr.evaluate(&request, access, None, &e.base_url);
                    let mut matched: Vec<Value> =
                        outcome.decision.iter().map(|d| json!({ "ruleId": d.rule_id, "rulesetId": d.ruleset_id })).collect();
                    matched.extend(outcome.header_rules.iter().map(|h| json!({ "ruleId": h.rule_id, "rulesetId": h.ruleset_id })));
                    Ok(json!({ "matchedRules": matched }))
                })
            }
            "setExtensionActionOptions" => {
                self.with_ext_mut(call, |e| {
                    if let Some(show) = options.get("displayActionCountAsBadgeText").and_then(Value::as_bool) {
                        e.dnr.show_action_count = show;
                    }
                    if let Some(update) = options.get("tabUpdate") {
                        let tab = update
                            .get("tabId")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| ApiError::invalid("tabUpdate.tabId must be an integer"))?;
                        let by = update
                            .get("increment")
                            .and_then(Value::as_i64)
                            .ok_or_else(|| ApiError::invalid("tabUpdate.increment must be an integer"))?;
                        let count = e.dnr.action_counts.entry(tab).or_default();
                        *count = (*count + by).max(0);
                    }
                    Ok(())
                })?;
                self.save_rules(id);
                self.host.dispatch_event(id, "__ui.changed", vec![json!("action")]);
                Ok(Value::Null)
            }
            "getMatchedRules" => {
                let tab = options.get("tabId").and_then(Value::as_i64);
                let since = options.get("minTimeStamp").and_then(Value::as_f64).map(|t| t as i64);
                let now = self.host.now_ms();
                let tab_url = tab.and_then(|t| self.browser.borrow().tab(t).map(|x| x.url.clone()));
                self.with_ext_mut(call, |e| {
                    if !e.permissions.has_api("declarativeNetRequestFeedback") {
                        // With activeTab alone only a tab the user invoked the extension on can be asked about.
                        let granted = match (tab, &tab_url) {
                            (Some(t), Some(url)) => e.permissions.has_api("activeTab") && e.permissions.has_host_for_tab(t, url),
                            _ => false,
                        };
                        if !granted {
                            return Err(ApiError::runtime(
                                "The extension must have the declarativeNetRequestFeedback permission or be granted the activeTab permission for the specified tab.",
                            ));
                        }
                    }
                    if !e.dnr.take_get_matched_quota(now) {
                        return Err(ApiError::runtime("This request exceeds the MAX_GETMATCHEDRULES_CALLS_PER_INTERVAL quota."));
                    }
                    let list: Vec<Value> = e
                        .dnr
                        .matched
                        .iter()
                        .filter(|m| tab.is_none_or(|t| m.tab_id == t) && since.is_none_or(|s| m.time_ms >= s))
                        .map(dnr::MatchedRule::to_json)
                        .collect();
                    Ok(json!({ "rulesMatchedInfo": list }))
                })
            }
            other => Err(ApiError::new(ErrorKind::Unhandled, format!("no handler for declarativeNetRequest.{other}"))),
        }
    }

    /// Static rules beyond each extension's guaranteed 30,000 come from one pool; this is what others left of it.
    fn static_rule_pool_left(&self, extension_id: &str) -> usize {
        let used: usize = self
            .extensions
            .borrow()
            .values()
            .filter(|e| e.id != extension_id)
            .map(|e| e.dnr.enabled_static_rule_count().saturating_sub(dnr::GUARANTEED_MINIMUM_STATIC_RULES))
            .sum();
        dnr::GLOBAL_STATIC_RULE_POOL.saturating_sub(used)
    }

    /// The matched-rule count shown on the toolbar button, when the extension asked for it and set no text for the tab.
    pub(crate) fn action_count_badge(ext: &Extension, tab: Option<i64>) -> Option<String> {
        if !ext.dnr.show_action_count {
            return None;
        }
        let tab = tab?;
        if ext.action.tabs.get(&tab).is_some_and(|p| p.badge_text.is_some()) {
            return None;
        }
        ext.dnr.action_counts.get(&tab).filter(|c| **c > 0).map(|c| c.to_string())
    }

    // ---- webNavigation ------------------------------------------------------------------------------------------------

    /// A navigation step, from the browser:
    ///
    /// `{ event, tabId, frameId, parentFrameId, url, timeStamp?, documentId?, transitionType?, transitionQualifiers?, error?,
    /// sourceTabId?, sourceFrameId?, replacedTabId? }` with `event` one of `beforeNavigate`, `committed`,
    /// `domContentLoaded`, `completed`, `errorOccurred`, `historyStateUpdated`, `referenceFragmentUpdated`,
    /// `createdNavigationTarget`, `tabReplaced`. Frame 0 is the tab's top frame; its `parentFrameId` is -1.
    pub fn navigation_event(&self, args: &Value) -> Value {
        let Some(event) = args.get("event").and_then(Value::as_str) else {
            return json!({ "error": "navigation needs an event" });
        };
        let name = match event {
            "beforeNavigate" => "webNavigation.onBeforeNavigate",
            "committed" => "webNavigation.onCommitted",
            "domContentLoaded" => "webNavigation.onDOMContentLoaded",
            "completed" => "webNavigation.onCompleted",
            "errorOccurred" => "webNavigation.onErrorOccurred",
            "historyStateUpdated" => "webNavigation.onHistoryStateUpdated",
            "referenceFragmentUpdated" => "webNavigation.onReferenceFragmentUpdated",
            "createdNavigationTarget" => "webNavigation.onCreatedNavigationTarget",
            "tabReplaced" => "webNavigation.onTabReplaced",
            other => return json!({ "error": format!("unknown navigation event '{other}'") }),
        };
        let int = |k: &str| args.get(k).and_then(Value::as_i64);
        let tab_id = int("tabId").unwrap_or(-1);
        let frame_id = int("frameId").unwrap_or(0);
        let parent = int("parentFrameId").unwrap_or(if frame_id == 0 { -1 } else { 0 });
        let url = args.get("url").and_then(Value::as_str).unwrap_or("");
        let time = args.get("timeStamp").and_then(Value::as_f64).unwrap_or(self.host.now_ms() as f64);
        let document_id = args.get("documentId").and_then(Value::as_str);

        let parent_document = {
            let mut net = self.network.borrow_mut();
            let frames = net.frames.entry(tab_id).or_default();
            match event {
                "committed" => {
                    if frame_id == 0 {
                        frames.clear();
                    } else {
                        // The frames inside the navigated frame are gone with its old document.
                        let mut gone = vec![frame_id];
                        while let Some(f) = gone.pop() {
                            let children: Vec<i64> = frames.iter().filter(|(_, v)| v.parent == f).map(|(k, _)| *k).collect();
                            for c in children {
                                frames.remove(&c);
                                gone.push(c);
                            }
                        }
                    }
                    frames.insert(frame_id, NavFrame { url: url.into(), parent, error: false, document_id: document_id.map(String::from) });
                }
                "historyStateUpdated" | "referenceFragmentUpdated" => {
                    frames.entry(frame_id).or_insert_with(|| NavFrame { parent, ..Default::default() }).url = url.into();
                }
                "errorOccurred" => {
                    let f = frames.entry(frame_id).or_insert_with(|| NavFrame { parent, ..Default::default() });
                    f.error = true;
                    if f.url.is_empty() {
                        f.url = url.into();
                    }
                }
                _ => {}
            }
            frames.get(&parent).and_then(|p| p.document_id.clone())
        };
        if event == "committed" && frame_id == 0 {
            // Counts on the badge are per page.
            let mut changed = vec![];
            for e in self.extensions.borrow_mut().values_mut() {
                if e.dnr.action_counts.remove(&tab_id).is_some() && e.dnr.show_action_count {
                    changed.push(e.id.clone());
                }
            }
            for id in changed {
                self.host.dispatch_event(&id, "__ui.changed", vec![json!("action")]);
            }
        }

        let build = |firefox: bool| -> Value {
            let mut d = Map::new();
            match event {
                "createdNavigationTarget" => {
                    d.insert("sourceTabId".into(), json!(int("sourceTabId").unwrap_or(-1)));
                    d.insert("sourceFrameId".into(), json!(int("sourceFrameId").unwrap_or(0)));
                    if !firefox {
                        d.insert("sourceProcessId".into(), json!(0));
                    }
                    d.insert("tabId".into(), json!(tab_id));
                    d.insert("url".into(), json!(url));
                    d.insert("timeStamp".into(), json!(time));
                    return Value::Object(d);
                }
                "tabReplaced" => {
                    return json!({ "replacedTabId": int("replacedTabId").unwrap_or(-1), "tabId": tab_id, "timeStamp": time });
                }
                _ => {}
            }
            d.insert("tabId".into(), json!(tab_id));
            d.insert("url".into(), json!(url));
            d.insert("frameId".into(), json!(frame_id));
            d.insert("parentFrameId".into(), json!(parent));
            d.insert("timeStamp".into(), json!(time));
            if !firefox {
                d.insert("processId".into(), json!(0));
                d.insert("frameType".into(), json!(if frame_id == 0 { "outermost_frame" } else { "sub_frame" }));
                d.insert("documentLifecycle".into(), json!("active"));
                if event != "beforeNavigate"
                    && let Some(id) = document_id
                {
                    d.insert("documentId".into(), json!(id));
                }
                if let Some(p) = &parent_document {
                    d.insert("parentDocumentId".into(), json!(p));
                }
            }
            if matches!(event, "committed" | "historyStateUpdated" | "referenceFragmentUpdated") {
                d.insert("transitionType".into(), args.get("transitionType").filter(|v| v.is_string()).cloned().unwrap_or(json!("link")));
                d.insert(
                    "transitionQualifiers".into(),
                    args.get("transitionQualifiers").filter(|v| v.is_array()).cloned().unwrap_or(json!([])),
                );
            }
            if event == "errorOccurred" {
                d.insert("error".into(), args.get("error").filter(|v| v.is_string()).cloned().unwrap_or(json!("net::ERR_FAILED")));
            }
            Value::Object(d)
        };
        let targets: Vec<(String, bool)> = self
            .extensions
            .borrow()
            .values()
            .filter(|e| e.permissions.has_api("webNavigation"))
            .map(|e| (e.id.clone(), crate::browser_api::is_firefox(e)))
            .collect();
        for (id, firefox) in targets {
            self.host.dispatch_event(&id, name, vec![build(firefox)]);
        }
        Value::Null
    }

    fn web_navigation(&self, member: &Member, call: &ApiCall, reply: Reply) {
        let details = call.arg(0).clone();
        let Some(tab_id) = details.get("tabId").and_then(Value::as_i64) else {
            return reply(Err(ApiError::invalid("tabId must be an integer")));
        };
        let firefox = self.with_ext(call, |e| Ok(crate::browser_api::is_firefox(e))).unwrap_or(false);
        let Some(tab_url) = self.browser.borrow().tab(tab_id).map(|t| t.url.clone()) else {
            return reply(Err(ApiError::runtime(if firefox {
                format!("Invalid tab ID: {tab_id}")
            } else {
                format!("No tab with id: {tab_id}.")
            })));
        };
        let one = member.name == "getFrame";
        let wanted_frame = details.get("frameId").and_then(Value::as_i64);
        let wanted_document = details.get("documentId").and_then(Value::as_str).map(String::from);
        if one && wanted_frame.is_none() && wanted_document.is_none() {
            return reply(Err(ApiError::invalid("Either documentId or frameId must be specified.")));
        }
        let this = self.clone();
        self.host_call(call, "host.tabs.frames", json!({ "tabId": tab_id }), reply, move |_, listed| {
            let net = this.network.borrow();
            let known = net.frames.get(&tab_id);
            let mut frames: Vec<(i64, i64, String, Option<String>)> = listed
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    let id = f.get("frameId")?.as_i64()?;
                    Some((
                        id,
                        f.get("parentFrameId").and_then(Value::as_i64).unwrap_or(if id == 0 { -1 } else { 0 }),
                        f.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
                        f.get("documentId").and_then(Value::as_str).map(String::from),
                    ))
                })
                .collect();
            if frames.is_empty() {
                // The browser could not list them (a tab that has not loaded): what navigation events told, or the tab.
                frames = match known.filter(|k| !k.is_empty()) {
                    Some(k) => k.iter().map(|(id, f)| (*id, f.parent, f.url.clone(), f.document_id.clone())).collect(),
                    None => vec![(0, -1, tab_url.clone(), None)],
                };
                frames.sort_by_key(|f| f.0);
            }
            let describe = |(id, parent, url, document): &(i64, i64, String, Option<String>), with_ids: bool| -> Value {
                let mut d = Map::new();
                d.insert("errorOccurred".into(), json!(known.and_then(|k| k.get(id)).is_some_and(|f| f.error)));
                d.insert("url".into(), json!(url));
                d.insert("parentFrameId".into(), json!(parent));
                if with_ids {
                    d.insert("frameId".into(), json!(id));
                    d.insert("tabId".into(), json!(tab_id));
                }
                if !firefox {
                    if with_ids {
                        d.insert("processId".into(), json!(0));
                    }
                    d.insert("frameType".into(), json!(if *id == 0 { "outermost_frame" } else { "sub_frame" }));
                    d.insert("documentLifecycle".into(), json!("active"));
                    if let Some(doc) = document {
                        d.insert("documentId".into(), json!(doc));
                    }
                    if let Some(p) = frames.iter().find(|f| f.0 == *parent).and_then(|f| f.3.clone()) {
                        d.insert("parentDocumentId".into(), json!(p));
                    }
                }
                Value::Object(d)
            };
            if !one {
                return Ok(Value::Array(frames.iter().map(|f| describe(f, true)).collect()));
            }
            let found = frames.iter().find(|f| match (&wanted_document, wanted_frame) {
                (Some(doc), _) => f.3.as_ref() == Some(doc),
                (None, Some(id)) => f.0 == id,
                (None, None) => false,
            });
            // Firefox's getFrame result names the frame too.
            Ok(found.map_or(Value::Null, |f| describe(f, firefox)))
        });
    }
}

fn class_blocks(a: ActionType) -> bool {
    matches!(a, ActionType::Block | ActionType::Redirect | ActionType::UpgradeScheme)
}

/// The `details` object of a `webRequest` event, as the listener's browser shapes it.
fn listener_details(stage: Stage, details: &Value, request: &Request, l: &Listener, firefox: bool) -> Value {
    let mut d = Map::new();
    let copy = |d: &mut Map<String, Value>, key: &str| {
        if let Some(v) = details.get(key).filter(|v| !v.is_null()) {
            d.insert(key.into(), v.clone());
        }
    };
    d.insert("requestId".into(), details.get("requestId").cloned().unwrap_or(json!("0")));
    d.insert("url".into(), json!(request.url));
    d.insert("method".into(), json!(details.get("method").and_then(Value::as_str).unwrap_or("GET").to_ascii_uppercase()));
    d.insert("frameId".into(), json!(details.get("frameId").and_then(Value::as_i64).unwrap_or(0)));
    d.insert("parentFrameId".into(), json!(details.get("parentFrameId").and_then(Value::as_i64).unwrap_or(-1)));
    d.insert("tabId".into(), json!(request.tab_id));
    let type_name = match (firefox, request.resource_type) {
        (true, ResourceType::Ping) => "beacon",
        (_, t) => t.as_str(),
    };
    d.insert("type".into(), json!(type_name));
    d.insert("timeStamp".into(), details.get("timeStamp").filter(|v| v.is_number()).cloned().unwrap_or(json!(0)));
    if firefox {
        copy(&mut d, "documentUrl");
        match details.get("originUrl").or(details.get("initiator")).filter(|v| v.is_string()) {
            Some(v) => d.insert("originUrl".into(), v.clone()),
            None => None,
        };
        d.insert("thirdParty".into(), json!(request.third_party));
        d.insert("incognito".into(), json!(details.get("incognito").and_then(Value::as_bool).unwrap_or(false)));
        d.insert("cookieStoreId".into(), json!("firefox-default"));
        d.insert("frameAncestors".into(), details.get("frameAncestors").filter(|v| v.is_array()).cloned().unwrap_or(json!([])));
        d.insert("urlClassification".into(), json!({ "firstParty": [], "thirdParty": [] }));
        d.insert("requestSize".into(), json!(0));
        d.insert("responseSize".into(), json!(0));
        d.insert("proxyInfo".into(), Value::Null);
    } else {
        copy(&mut d, "initiator");
        copy(&mut d, "documentId");
        copy(&mut d, "parentDocumentId");
        d.insert("frameType".into(), json!(if d["frameId"] == json!(0) { "outermost_frame" } else { "sub_frame" }));
        d.insert("documentLifecycle".into(), json!("active"));
    }
    if stage == Stage::BeforeRequest && l.request_body {
        copy(&mut d, "requestBody");
    }
    if matches!(stage, Stage::BeforeSendHeaders | Stage::SendHeaders) && l.request_headers {
        copy(&mut d, "requestHeaders");
    }
    let after_response =
        matches!(stage, Stage::HeadersReceived | Stage::AuthRequired | Stage::BeforeRedirect | Stage::ResponseStarted | Stage::Completed);
    if after_response {
        d.insert("statusCode".into(), details.get("statusCode").filter(|v| v.is_number()).cloned().unwrap_or(json!(0)));
        d.insert("statusLine".into(), details.get("statusLine").filter(|v| v.is_string()).cloned().unwrap_or(json!("")));
        if l.response_headers {
            copy(&mut d, "responseHeaders");
        }
    }
    if matches!(stage, Stage::BeforeRedirect | Stage::ResponseStarted | Stage::Completed | Stage::ErrorOccurred) {
        copy(&mut d, "ip");
        d.insert("fromCache".into(), json!(details.get("fromCache").and_then(Value::as_bool).unwrap_or(false)));
    }
    match stage {
        Stage::BeforeRedirect => copy(&mut d, "redirectUrl"),
        Stage::ErrorOccurred => {
            d.insert("error".into(), details.get("error").filter(|v| v.is_string()).cloned().unwrap_or(json!("net::ERR_FAILED")));
        }
        Stage::AuthRequired => {
            for key in ["scheme", "realm", "challenger", "isProxy"] {
                copy(&mut d, key);
            }
        }
        _ => {}
    }
    Value::Object(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::InstallOptions;
    use crate::dispatch::testutil::*;
    use crate::loader::Files;
    use std::cell::RefCell;
    use std::rc::Rc;

    const DNR: &str = r#"{"manifest_version":3,"name":"x","version":"1",
        "permissions":["declarativeNetRequest","declarativeNetRequestFeedback","webRequest","webNavigation"],
        "host_permissions":["*://*.site.test/*","*://*.ads.test/*"]}"#;
    const MV2: &str = r#"{"manifest_version":2,"name":"x","version":"1",
        "permissions":["webRequest","webRequestBlocking","<all_urls>"]}"#;

    /// Runs one stage; `None` while blocking listeners have not answered.
    fn stage(d: &Dispatcher, stage: &str, details: Value) -> Rc<RefCell<Option<Value>>> {
        let out = Rc::new(RefCell::new(None));
        let sink = out.clone();
        d.network_event(stage, &details, Box::new(move |r| *sink.borrow_mut() = Some(r.unwrap())));
        out
    }

    fn now(d: &Dispatcher, name: &str, details: Value) -> Value {
        stage(d, name, details).borrow_mut().take().expect("answered at once")
    }

    fn request(url: &str, kind: &str) -> Value {
        json!({"requestId": "7", "url": url, "method": "GET", "type": kind, "tabId": 1, "frameId": 0, "parentFrameId": -1,
               "initiator": "https://www.site.test", "timeStamp": 5.0})
    }

    fn add_rules(d: &Dispatcher, rules: Value) {
        call(d, "declarativeNetRequest.updateDynamicRules", vec![json!({ "addRules": rules })]).unwrap();
    }

    #[test]
    fn rules_block_redirect_and_are_remembered() {
        let (d, host) = setup(DNR);
        assert_eq!(d.network_interest()["any"], false);
        add_rules(
            &d,
            json!([
                {"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test^"}},
                {"id": 2, "action": {"type": "redirect", "redirect": {"extensionPath": "/empty.js"}}, "condition": {"urlFilter": "||cdn.site.test/tracker.js"}},
                {"id": 3, "action": {"type": "redirect", "redirect": {"url": "https://site.test/"}}, "condition": {"urlFilter": "||elsewhere.test"}}
            ]),
        );
        assert_eq!(d.network_interest()["stages"], json!(["beforeRequest"]));
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "__network.changed"));
        assert_eq!(now(&d, "beforeRequest", request("https://ads.test/a.js", "script")), json!({"cancel": true}));
        assert_eq!(
            now(&d, "beforeRequest", request("https://cdn.site.test/tracker.js", "script")),
            json!({"redirectUrl": format!("chrome-extension://{ID}/empty.js")})
        );
        assert_eq!(now(&d, "beforeRequest", request("https://elsewhere.test/x", "script")), json!({}), "no host permission, no redirect");
        assert_eq!(
            now(&d, "beforeRequest", request("https://ads.test/", "main_frame")),
            json!({}),
            "navigations are not covered by default"
        );

        let matched = call(&d, "declarativeNetRequest.getMatchedRules", vec![json!({"tabId": 1})]).unwrap();
        assert_eq!(matched["rulesMatchedInfo"].as_array().unwrap().len(), 2);
        assert_eq!(matched["rulesMatchedInfo"][0]["rule"], json!({"ruleId": 1, "rulesetId": "_dynamic"}));
        let debug: Vec<_> =
            host.events.borrow().iter().filter(|(_, e, _)| e == "declarativeNetRequest.onRuleMatchedDebug").cloned().collect();
        assert_eq!(debug.len(), 2);
        assert_eq!(debug[0].2[0]["request"]["url"], "https://ads.test/a.js");

        // The rules survive a restart.
        let saved = host.storage.borrow().get(&(ID.to_string(), "dnr".to_string())).cloned().unwrap();
        assert!(saved.contains("empty.js"));
        let rules = call(&d, "declarativeNetRequest.getDynamicRules", vec![json!({"ruleIds": [2]})]).unwrap();
        assert_eq!(rules.as_array().unwrap().len(), 1);
        assert_eq!(rules[0]["priority"], 1);
    }

    #[test]
    fn api_functions() {
        let (d, _) = setup(DNR);
        let ok = call(&d, "declarativeNetRequest.isRegexSupported", vec![json!({"regex": "^a(b|c)+$"})]).unwrap();
        assert_eq!(ok, json!({"isSupported": true}));
        let bad = call(&d, "declarativeNetRequest.isRegexSupported", vec![json!({"regex": "a("})]).unwrap();
        assert_eq!(bad, json!({"isSupported": false, "reason": "syntaxError"}));
        assert_eq!(call(&d, "declarativeNetRequest.getEnabledRulesets", vec![]).unwrap(), json!([]));
        assert_eq!(call(&d, "declarativeNetRequest.getAvailableStaticRuleCount", vec![]).unwrap(), json!(330_000));
        call(
            &d,
            "declarativeNetRequest.updateSessionRules",
            vec![json!({"addRules": [{"id": 4, "action": {"type": "block"}, "condition": {"urlFilter": "x", "tabIds": [1]}}]})],
        )
        .unwrap();
        assert_eq!(call(&d, "declarativeNetRequest.getSessionRules", vec![]).unwrap().as_array().unwrap().len(), 1);
        let err = call(&d, "declarativeNetRequest.updateDynamicRules", vec![json!({"addRules": [{"id": 4, "action": {"type": "what"}}]})])
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Runtime);
        let outcome =
            call(&d, "declarativeNetRequest.testMatchOutcome", vec![json!({"url": "https://a.site.test/x", "type": "script", "tabId": 1})])
                .unwrap();
        assert_eq!(outcome, json!({"matchedRules": [{"ruleId": 4, "rulesetId": "_session"}]}));
        assert!(call(&d, "declarativeNetRequest.getDisabledRuleIds", vec![json!({"rulesetId": "none"})]).is_err());
    }

    #[test]
    fn static_rulesets_come_from_the_package() {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host.clone(), crate::registry::Registry::builtin());
        let manifest = r#"{"manifest_version":3,"name":"x","version":"1","permissions":["declarativeNetRequest"],
            "declarative_net_request":{"rule_resources":[{"id":"main","enabled":true,"path":"rules/main.json"},{"id":"extra","enabled":false,"path":"rules/extra.json"}]}}"#;
        let mut files = Files::new();
        files.insert("rules/main.json", br#"[{"id":1,"action":{"type":"block"},"condition":{"urlFilter":"||ads.test^"}}]"#.to_vec());
        files.insert("rules/extra.json", br#"[{"id":1,"action":{"type":"block"},"condition":{"urlFilter":"||more.test^"}}]"#.to_vec());
        let install = |d: &Dispatcher, files: Files| {
            let m = crate::manifest::parse_manifest(manifest).manifest.unwrap();
            d.install(ID, m, InstallOptions { base_url: format!("chrome-extension://{ID}/"), locales: vec![], files });
        };
        install(&d, files.clone());
        assert_eq!(
            now(&d, "beforeRequest", request("https://ads.test/a.js", "script")),
            json!({"cancel": true}),
            "blocking needs no host permission"
        );
        assert_eq!(now(&d, "beforeRequest", request("https://more.test/a.js", "script")), json!({}));
        call(&d, "declarativeNetRequest.updateEnabledRulesets", vec![json!({"enableRulesetIds": ["extra"]})]).unwrap();
        call(&d, "declarativeNetRequest.updateStaticRules", vec![json!({"rulesetId": "main", "disableRuleIds": [1]})]).unwrap();
        assert_eq!(now(&d, "beforeRequest", request("https://more.test/a.js", "script")), json!({"cancel": true}));
        assert_eq!(now(&d, "beforeRequest", request("https://ads.test/a.js", "script")), json!({}));
        assert_eq!(call(&d, "declarativeNetRequest.getAvailableStaticRuleCount", vec![]).unwrap(), json!(329_998));

        // After a restart the same rulesets are on and the same rule is off.
        let d2 = Dispatcher::new(host.clone(), crate::registry::Registry::builtin());
        install(&d2, files);
        assert_eq!(call(&d2, "declarativeNetRequest.getEnabledRulesets", vec![]).unwrap(), json!(["main", "extra"]));
        assert_eq!(call(&d2, "declarativeNetRequest.getDisabledRuleIds", vec![json!({"rulesetId": "main"})]).unwrap(), json!([1]));
        assert_eq!(now(&d2, "beforeRequest", request("https://more.test/a.js", "script")), json!({"cancel": true}));
    }

    #[test]
    fn header_rules_change_request_and_response_headers() {
        let (d, _) = setup(DNR);
        add_rules(
            &d,
            json!([
                {"id": 1, "priority": 2, "action": {"type": "modifyHeaders",
                    "requestHeaders": [{"header": "Referer", "operation": "remove"}, {"header": "X-Dnt", "operation": "set", "value": "1"}],
                    "responseHeaders": [{"header": "Set-Cookie", "operation": "remove"}]},
                 "condition": {"urlFilter": "||site.test^", "resourceTypes": ["xmlhttprequest"]}},
                {"id": 2, "priority": 1, "action": {"type": "modifyHeaders", "requestHeaders": [{"header": "x-dnt", "operation": "set", "value": "lower priority"}]},
                 "condition": {"urlFilter": "||site.test^", "resourceTypes": ["xmlhttprequest"]}}
            ]),
        );
        let interest = d.network_interest();
        assert_eq!(interest["stages"], json!(["beforeRequest", "beforeSendHeaders", "headersReceived"]));
        let mut details = request("https://api.site.test/x", "xmlhttprequest");
        details["requestHeaders"] = json!([{"name": "Referer", "value": "https://www.site.test/"}, {"name": "Accept", "value": "*/*"}]);
        assert_eq!(
            now(&d, "beforeSendHeaders", details.clone()),
            json!({"requestHeaders": [{"name": "Accept", "value": "*/*"}, {"name": "x-dnt", "value": "1"}]})
        );
        details["responseHeaders"] = json!([{"name": "set-cookie", "value": "a=1"}, {"name": "content-type", "value": "text/plain"}]);
        assert_eq!(
            now(&d, "headersReceived", details.clone()),
            json!({"responseHeaders": [{"name": "content-type", "value": "text/plain"}]})
        );
        details["url"] = json!("https://api.site.test/x.png");
        details["type"] = json!("image");
        assert_eq!(now(&d, "beforeSendHeaders", details), json!({}), "other resource types are untouched");
    }

    #[test]
    fn allow_all_requests_covers_the_frame_and_its_children() {
        let (d, _) = setup(DNR);
        add_rules(
            &d,
            json!([
                {"id": 1, "priority": 1, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test^"}},
                {"id": 2, "priority": 5, "action": {"type": "allowAllRequests"}, "condition": {"urlFilter": "||trusted.site.test^", "resourceTypes": ["main_frame", "sub_frame"]}}
            ]),
        );
        let in_frame = |frame: i64| {
            let mut r = request("https://ads.test/a.js", "script");
            r["frameId"] = json!(frame);
            r
        };
        now(&d, "beforeRequest", request("https://www.site.test/", "main_frame"));
        assert_eq!(now(&d, "beforeRequest", in_frame(0)), json!({"cancel": true}));
        // A trusted subframe (frame 4), and a frame nested in it (frame 9).
        let mut sub = request("https://trusted.site.test/widget", "sub_frame");
        sub["frameId"] = json!(4);
        sub["parentFrameId"] = json!(0);
        now(&d, "beforeRequest", sub);
        let mut nested = request("https://other.site.test/inner", "sub_frame");
        nested["frameId"] = json!(9);
        nested["parentFrameId"] = json!(4);
        now(&d, "beforeRequest", nested);
        assert_eq!(now(&d, "beforeRequest", in_frame(4)), json!({}));
        assert_eq!(now(&d, "beforeRequest", in_frame(9)), json!({}));
        assert_eq!(now(&d, "beforeRequest", in_frame(0)), json!({"cancel": true}), "the page around it is not covered");
        // Navigating the tab forgets the frames.
        now(&d, "beforeRequest", request("https://www.site.test/next", "main_frame"));
        assert_eq!(now(&d, "beforeRequest", in_frame(4)), json!({"cancel": true}));
    }

    #[test]
    fn a_block_from_any_extension_wins_and_the_badge_counts() {
        let (d, host) = setup(DNR);
        let other = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let m = crate::manifest::parse_manifest(DNR).manifest.unwrap();
        d.install(other, m, InstallOptions { base_url: format!("chrome-extension://{other}/"), locales: vec![], files: Files::new() });
        add_rules(&d, json!([{"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test^"}}]));
        // The other extension, installed later, allows the same request.
        let allow = ApiCall {
            extension_id: other.into(),
            function: "declarativeNetRequest.updateDynamicRules".into(),
            args: vec![
                json!({"addRules": [{"id": 1, "priority": 9, "action": {"type": "allow"}, "condition": {"urlFilter": "||ads.test^"}}]}),
            ],
            caller: Caller::background(),
        };
        d.dispatch(allow, Box::new(|r| assert!(r.is_ok())));
        assert_eq!(now(&d, "beforeRequest", request("https://ads.test/a.js", "script")), json!({"cancel": true}));

        call(&d, "declarativeNetRequest.setExtensionActionOptions", vec![json!({"displayActionCountAsBadgeText": true})]).unwrap();
        now(&d, "beforeRequest", request("https://ads.test/b.js", "script"));
        now(&d, "beforeRequest", request("https://ads.test/c.js", "script"));
        let exts = d.extensions.borrow();
        assert_eq!(Dispatcher::action_count_badge(&exts[ID], Some(1)).as_deref(), Some("2"));
        assert_eq!(Dispatcher::action_count_badge(&exts[ID], Some(2)), None);
        assert!(host.events.borrow().iter().any(|(id, e, _)| id == ID && e == "__ui.changed"));
    }

    fn listen(d: &Dispatcher, ctx: u64, event: &str, id: u64, filter: Value, specs: Value) -> ApiResult {
        call_ctx(d, ctx, ContextKind::Background, "__webRequest.addListener", vec![json!(event), json!(id), filter, specs])
    }

    #[test]
    fn web_request_listeners_observe_matching_requests() {
        let (d, host) = setup(DNR);
        register(&d, 1, ContextKind::Background, None, None);
        listen(&d, 1, "webRequest.onBeforeRequest", 1, json!({"urls": ["*://*.site.test/*"], "types": ["script", "beacon"]}), json!([]))
            .unwrap();
        listen(&d, 1, "webRequest.onCompleted", 2, json!({"urls": ["<all_urls>"], "tabId": 99}), json!(["responseHeaders"])).unwrap();
        assert_eq!(d.network_interest()["stages"], json!(["beforeRequest", "completed"]));
        assert!(listen(&d, 1, "webRequest.onBeforeRequest", 3, json!({}), json!([])).is_err(), "urls is required");
        // uBlock Origin watches requests for its own files.
        listen(
            &d,
            1,
            "webRequest.onSendHeaders",
            9,
            json!({"urls": [format!("chrome-extension://{ID}/web_accessible_resources/*")]}),
            json!([]),
        )
        .unwrap();
        call_ctx(&d, 1, ContextKind::Background, "__webRequest.removeListener", vec![json!("webRequest.onSendHeaders"), json!(9)]).unwrap();
        assert!(
            listen(&d, 1, "webRequest.onBeforeRequest", 3, json!({"urls": ["<all_urls>"]}), json!(["blocking"])).is_err(),
            "no blocking in Chrome MV3"
        );

        assert_eq!(now(&d, "beforeRequest", request("https://a.site.test/app.js", "script")), json!({}));
        assert_eq!(now(&d, "beforeRequest", request("https://a.site.test/app.css", "stylesheet")), json!({}));
        assert_eq!(now(&d, "beforeRequest", request("https://unpermitted.test/app.js", "script")), json!({}));
        assert_eq!(now(&d, "completed", request("https://a.site.test/app.js", "script")), json!({}), "another tab's filter");
        let seen = delivered_to(&host, 1);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["event"], "webRequest.onBeforeRequest");
        assert_eq!(seen[0]["listenerId"], 1);
        assert_eq!(seen[0]["replyTo"], Value::Null);
        let details = &seen[0]["details"];
        assert_eq!(details["url"], "https://a.site.test/app.js");
        assert_eq!(details["initiator"], "https://www.site.test");
        assert_eq!(details["type"], "script");
        assert_eq!(details["frameType"], "outermost_frame");

        call_ctx(&d, 1, ContextKind::Background, "__webRequest.removeListener", vec![json!("webRequest.onBeforeRequest"), json!(1)])
            .unwrap();
        now(&d, "beforeRequest", request("https://a.site.test/app.js", "script"));
        assert_eq!(delivered_to(&host, 1).len(), 1);
    }

    #[test]
    fn blocking_listeners_decide() {
        let (d, host) = setup(MV2);
        register(&d, 1, ContextKind::Background, None, None);
        register(&d, 2, ContextKind::ExtensionPage, None, None);
        listen(&d, 1, "webRequest.onBeforeRequest", 1, json!({"urls": ["<all_urls>"]}), json!(["blocking"])).unwrap();
        listen(&d, 2, "webRequest.onBeforeRequest", 1, json!({"urls": ["<all_urls>"]}), json!(["blocking"])).unwrap();
        listen(&d, 1, "webRequest.onBeforeSendHeaders", 2, json!({"urls": ["<all_urls>"]}), json!(["blocking", "requestHeaders"])).unwrap();
        listen(&d, 1, "webRequest.onHeadersReceived", 3, json!({"urls": ["<all_urls>"]}), json!(["blocking"])).unwrap();

        let answer = |ctx: u64, envelope: &Value, response: Value| {
            call_ctx(
                &d,
                ctx,
                ContextKind::Background,
                "__webRequest.reply",
                vec![envelope["replyTo"].clone(), envelope["listenerId"].clone(), response],
            )
            .unwrap();
        };
        // Both listeners must answer before the request goes on; one cancel is enough.
        let slot = stage(&d, "beforeRequest", request("https://x.test/a.js", "script"));
        assert!(slot.borrow().is_none());
        let first = delivered_to(&host, 1)[0].clone();
        assert!(first["replyTo"].is_number());
        answer(1, &first, json!({}));
        assert!(slot.borrow().is_none());
        answer(2, &delivered_to(&host, 2)[0], json!({"cancel": true}));
        assert_eq!(slot.borrow_mut().take(), Some(json!({"cancel": true})));

        // A redirect.
        let slot = stage(&d, "beforeRequest", request("https://x.test/b.js", "script"));
        answer(1, &delivered_to(&host, 1)[1], json!({"redirectUrl": "https://y.test/b.js"}));
        answer(2, &delivered_to(&host, 2)[1], Value::Null);
        assert_eq!(slot.borrow_mut().take(), Some(json!({"redirectUrl": "https://y.test/b.js"})));

        // Changed request headers; the details carry them because the listener asked.
        let mut details = request("https://x.test/c.js", "script");
        details["requestHeaders"] = json!([{"name": "User-Agent", "value": "Ladybird"}]);
        let slot = stage(&d, "beforeSendHeaders", details.clone());
        let envelope = delivered_to(&host, 1)[2].clone();
        assert_eq!(envelope["details"]["requestHeaders"][0]["name"], "User-Agent");
        answer(1, &envelope, json!({"requestHeaders": [{"name": "User-Agent", "value": "Other"}]}));
        assert_eq!(slot.borrow_mut().take(), Some(json!({"requestHeaders": [{"name": "User-Agent", "value": "Other"}]})));

        // A listener whose context goes away does not hold the request for ever.
        details["responseHeaders"] = json!([{"name": "content-type", "value": "text/html"}]);
        let slot = stage(&d, "headersReceived", details);
        assert!(slot.borrow().is_none());
        assert!(delivered_to(&host, 1)[3]["details"].get("responseHeaders").is_none(), "not asked for");
        d.context_destroyed(1);
        assert_eq!(slot.borrow_mut().take(), Some(json!({})));
        assert_eq!(d.network_interest()["stages"], json!(["beforeRequest"]), "context 2 still listens");
    }

    #[test]
    fn firefox_details_and_security_info() {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host.clone(), crate::registry::Registry::builtin());
        let m = crate::manifest::parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","permissions":["webRequest","webRequestBlocking"],"host_permissions":["<all_urls>"]}"#,
        )
        .manifest
        .unwrap();
        d.install(ID, m, InstallOptions { base_url: "moz-extension://1111/".into(), locales: vec![], files: Files::new() });
        register(&d, 1, ContextKind::Background, None, None);
        listen(&d, 1, "webRequest.onBeforeRequest", 1, json!({"urls": ["<all_urls>"]}), json!(["blocking"]))
            .expect("Firefox keeps blocking in MV3");
        let mut ping = request("http://x.test/p", "ping");
        ping["documentUrl"] = json!("https://www.site.test/page");
        let slot = stage(&d, "beforeRequest", ping);
        let envelope = delivered_to(&host, 1)[0].clone();
        assert_eq!(envelope["details"]["type"], "beacon");
        assert_eq!(envelope["details"]["originUrl"], "https://www.site.test");
        assert_eq!(envelope["details"]["documentUrl"], "https://www.site.test/page");
        assert_eq!(envelope["details"]["thirdParty"], true);
        assert!(envelope["details"].get("initiator").is_none());
        call_ctx(
            &d,
            1,
            ContextKind::Background,
            "__webRequest.reply",
            vec![envelope["replyTo"].clone(), json!(1), json!({"upgradeToSecure": true})],
        )
        .unwrap();
        assert_eq!(slot.borrow_mut().take(), Some(json!({"redirectUrl": "https://x.test/p"})));

        let mut received = request("https://x.test/p", "xmlhttprequest");
        received["securityInfo"] = json!({"state": "secure", "protocolVersion": "TLSv1.3"});
        now(&d, "headersReceived", received.clone());
        assert_eq!(call(&d, "webRequest.getSecurityInfo", vec![json!("7"), json!({})]).unwrap()["protocolVersion"], "TLSv1.3");
        now(&d, "completed", received);
        assert_eq!(call(&d, "webRequest.getSecurityInfo", vec![json!("7"), json!({})]).unwrap(), json!({"state": "insecure"}));
    }

    #[test]
    fn navigation_events_and_frames() {
        let (d, host) = setup(DNR);
        d.host_event(
            "browserState",
            &json!({"windows": [{"id": 1, "type": "normal", "state": "normal", "focused": true, "tabs": [{"id": 1, "active": true, "url": "https://www.site.test/", "title": "t", "status": "complete"}]}], "lastFocusedWindowId": 1}),
        );
        let nav = |event: &str, frame: i64, url: &str| {
            let r = d.host_event(
                "navigation",
                &json!({"event": event, "tabId": 1, "frameId": frame, "url": url, "documentId": format!("doc{frame}"), "timeStamp": 10.0}),
            );
            assert_eq!(r, Value::Null);
        };
        nav("beforeNavigate", 0, "https://www.site.test/");
        nav("committed", 0, "https://www.site.test/");
        nav("committed", 3, "https://frame.site.test/");
        nav("errorOccurred", 3, "https://frame.site.test/");
        nav("domContentLoaded", 0, "https://www.site.test/");
        nav("completed", 0, "https://www.site.test/");
        nav("historyStateUpdated", 0, "https://www.site.test/next");
        let events: Vec<(String, Value)> = host
            .events
            .borrow()
            .iter()
            .filter(|(_, e, _)| e.starts_with("webNavigation."))
            .map(|(_, e, a)| (e.clone(), a[0].clone()))
            .collect();
        let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
        assert_eq!(
            names,
            [
                "webNavigation.onBeforeNavigate",
                "webNavigation.onCommitted",
                "webNavigation.onCommitted",
                "webNavigation.onErrorOccurred",
                "webNavigation.onDOMContentLoaded",
                "webNavigation.onCompleted",
                "webNavigation.onHistoryStateUpdated"
            ]
        );
        assert!(events[0].1.get("documentId").is_none(), "no document yet before the navigation commits");
        assert_eq!(events[1].1["transitionType"], "link");
        assert_eq!(events[1].1["frameType"], "outermost_frame");
        assert_eq!(events[2].1["parentFrameId"], 0);
        assert_eq!(events[2].1["parentDocumentId"], "doc0");
        assert_eq!(events[3].1["error"], "net::ERR_FAILED");

        // The mock browser cannot list frames, so the answer comes from what the events told.
        let frames = call(&d, "webNavigation.getAllFrames", vec![json!({"tabId": 1})]).unwrap();
        assert_eq!(frames.as_array().unwrap().len(), 2);
        assert_eq!(frames[0]["url"], "https://www.site.test/next");
        assert_eq!(frames[1]["errorOccurred"], true);
        assert_eq!(frames[1]["frameId"], 3);
        let frame = call(&d, "webNavigation.getFrame", vec![json!({"tabId": 1, "frameId": 3})]).unwrap();
        assert_eq!(frame["url"], "https://frame.site.test/");
        assert!(frame.get("frameId").is_none());
        assert_eq!(call(&d, "webNavigation.getFrame", vec![json!({"tabId": 1, "frameId": 8})]).unwrap(), Value::Null);
        assert!(call(&d, "webNavigation.getAllFrames", vec![json!({"tabId": 55})]).unwrap_err().message.contains("No tab with id"));

        // A new document in the top frame drops the old frames.
        nav("committed", 0, "https://www.site.test/other");
        assert_eq!(call(&d, "webNavigation.getAllFrames", vec![json!({"tabId": 1})]).unwrap().as_array().unwrap().len(), 1);
    }
}
