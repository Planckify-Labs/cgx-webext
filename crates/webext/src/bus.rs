//! The message bus: contexts, long-lived ports and one-shot messages between the contexts of one extension.
//!
//! A *context* is one place extension code runs: the background page, a popup, an options page, or the content scripts of
//! one frame. The browser registers contexts as they come and go and carries opaque envelopes between them; every decision
//! (who receives a message, who answers first, what to do when nobody listens, what happens when a tab closes) is made here.
//!
//! The bus is a pure state machine: each operation returns [`Effect`]s (deliver an envelope to a context, complete a pending
//! call) that the dispatcher performs after releasing its borrows, which keeps the logic unit-testable without a browser.

use crate::host::{ApiError, ApiResult, ContextKind, ErrorKind, Reply};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub type ContextId = u64;

/// What Chrome and Firefox tell an extension when nothing is listening.
pub const NO_RECEIVER: &str = "Could not establish connection. Receiving end does not exist.";
pub const PORT_CLOSED: &str = "Attempting to use a disconnected port object";
const MAX_PORTS_PER_EXTENSION: usize = 10_000;
const MAX_PENDING_MESSAGES_PER_EXTENSION: usize = 10_000;
const MAX_PORT_ID_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct ContextInfo {
    pub id: ContextId,
    pub extension_id: String,
    pub kind: ContextKind,
    pub tab_id: Option<i64>,
    pub frame_id: Option<i64>,
    pub url: Option<String>,
}

/// Who a connection or message is addressed to.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// `runtime.connect` / `runtime.sendMessage`: the extension's other background and extension-page contexts.
    Runtime,
    /// `tabs.connect` / `tabs.sendMessage`: the content scripts of a tab, or of one frame in it.
    Tab { tab_id: i64, frame_id: Option<i64> },
}

/// Something the dispatcher must do after a bus operation.
pub enum Effect {
    /// Hand an envelope to a context. The browser must preserve the order of envelopes to one context.
    Deliver(ContextId, Value),
    /// Finish a pending API call.
    Complete(Reply, ApiResult),
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::Deliver(ctx, v) => write!(f, "Deliver({ctx}, {v})"),
            Effect::Complete(_, r) => write!(f, "Complete(<reply>, {r:?})"),
        }
    }
}

struct Port {
    extension_id: String,
    sender: ContextId,
    /// Targets that have not yet said whether they have an `onConnect` listener.
    pending: HashSet<ContextId>,
    /// Targets that accepted the connection.
    receivers: HashSet<ContextId>,
    ever_accepted: bool,
}

struct Request {
    extension_id: String,
    sender: ContextId,
    waiting: HashSet<ContextId>,
    any_listener: bool,
    reply: Option<Reply>,
}

#[derive(Default)]
pub struct Bus {
    contexts: HashMap<ContextId, ContextInfo>,
    ports: HashMap<String, Port>,
    requests: HashMap<u64, Request>,
    next_request: u64,
    next_port: u64,
}

fn sorted(set: &HashSet<ContextId>) -> Vec<ContextId> {
    let mut v: Vec<_> = set.iter().copied().collect();
    v.sort_unstable();
    v
}

fn disconnect_envelope(port_id: &str, error: Option<&str>) -> Value {
    json!({ "type": "port_disconnect", "portId": port_id, "error": error })
}

impl Bus {
    pub fn new() -> Bus {
        Bus::default()
    }

    /// A port id for callers that do not choose their own (the shim does).
    pub fn fresh_port_id(&mut self, sender: ContextId) -> String {
        self.next_port += 1;
        format!("{sender}.c{}", self.next_port)
    }

    pub fn context(&self, id: ContextId) -> Option<&ContextInfo> {
        self.contexts.get(&id)
    }

    pub fn contexts_of<'a>(&'a self, extension_id: &'a str) -> impl Iterator<Item = &'a ContextInfo> + 'a {
        self.contexts.values().filter(move |c| c.extension_id == extension_id)
    }

    pub fn context_created(&mut self, info: ContextInfo) -> Result<(), ApiError> {
        if self.contexts.contains_key(&info.id) {
            return Err(ApiError::invalid(format!("context {} already exists", info.id)));
        }
        self.contexts.insert(info.id, info);
        Ok(())
    }

    fn targets(&self, extension_id: &str, sender: ContextId, target: &Target) -> Vec<ContextId> {
        let mut ids: Vec<ContextId> = self
            .contexts
            .values()
            .filter(|c| c.extension_id == extension_id && c.id != sender)
            .filter(|c| match target {
                Target::Runtime => matches!(c.kind, ContextKind::Background | ContextKind::ExtensionPage),
                Target::Tab { tab_id, frame_id } => {
                    c.kind == ContextKind::ContentScript && c.tab_id == Some(*tab_id) && frame_id.is_none_or(|f| c.frame_id == Some(f))
                }
            })
            .map(|c| c.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn sender_context(&self, id: ContextId) -> Result<&ContextInfo, ApiError> {
        self.contexts.get(&id).ok_or_else(|| ApiError::runtime("Extension context invalidated."))
    }

    // ---- Ports ------------------------------------------------------------------------------------------------

    /// Opens a port from `sender`. `port_id` is chosen by the sender's shim and must start with `<sender id>.` so that a
    /// context cannot claim another's ports.
    pub fn connect(
        &mut self,
        sender: ContextId,
        port_id: String,
        name: &str,
        target: Target,
        sender_json: Value,
    ) -> Result<Vec<Effect>, ApiError> {
        let ctx = self.sender_context(sender)?;
        let extension_id = ctx.extension_id.clone();
        if port_id.len() > MAX_PORT_ID_LEN || !port_id.starts_with(&format!("{sender}.")) {
            return Err(ApiError::invalid("invalid port id"));
        }
        if self.ports.contains_key(&port_id) {
            return Err(ApiError::invalid("port id already in use"));
        }
        if self.ports.values().filter(|p| p.extension_id == extension_id).count() >= MAX_PORTS_PER_EXTENSION {
            return Err(ApiError::runtime("Too many open ports."));
        }

        let targets = self.targets(&extension_id, sender, &target);
        if targets.is_empty() {
            return Ok(vec![Effect::Deliver(sender, disconnect_envelope(&port_id, Some(NO_RECEIVER)))]);
        }
        let effects = targets
            .iter()
            .map(|&t| Effect::Deliver(t, json!({ "type": "connect", "portId": port_id, "name": name, "sender": sender_json })))
            .collect();
        self.ports.insert(
            port_id,
            Port { extension_id, sender, pending: targets.into_iter().collect(), receivers: HashSet::new(), ever_accepted: false },
        );
        Ok(effects)
    }

    /// A target says whether it has an `onConnect` listener (and therefore created its end of the port).
    pub fn accept(&mut self, ctx: ContextId, port_id: &str, accepted: bool) -> Vec<Effect> {
        let Some(port) = self.ports.get_mut(port_id) else { return vec![] };
        if !port.pending.remove(&ctx) {
            return vec![];
        }
        if accepted {
            port.receivers.insert(ctx);
            port.ever_accepted = true;
        }
        if port.pending.is_empty() && port.receivers.is_empty() {
            let port = self.ports.remove(port_id).expect("just looked up");
            return vec![Effect::Deliver(port.sender, disconnect_envelope(port_id, Some(NO_RECEIVER)))];
        }
        vec![]
    }

    pub fn post(&mut self, ctx: ContextId, port_id: &str, message: Value) -> Result<Vec<Effect>, ApiError> {
        let port = self.ports.get(port_id).ok_or_else(|| ApiError::runtime(PORT_CLOSED))?;
        let envelope = json!({ "type": "port_message", "portId": port_id, "message": message });
        if ctx == port.sender {
            // Receivers that have not answered yet are included: their `connect` envelope was delivered first, and one that
            // turns out to have no listener ignores messages for a port it never created.
            let mut all = port.pending.clone();
            all.extend(&port.receivers);
            Ok(sorted(&all).into_iter().map(|t| Effect::Deliver(t, envelope.clone())).collect())
        } else if port.receivers.contains(&ctx) || port.pending.contains(&ctx) {
            Ok(vec![Effect::Deliver(port.sender, envelope)])
        } else {
            Err(ApiError::new(ErrorKind::PermissionDenied, "this context is not an end of that port"))
        }
    }

    pub fn disconnect(&mut self, ctx: ContextId, port_id: &str) -> Vec<Effect> {
        self.leave_port(ctx, port_id)
    }

    fn leave_port(&mut self, ctx: ContextId, port_id: &str) -> Vec<Effect> {
        let Some(port) = self.ports.get_mut(port_id) else { return vec![] };
        if ctx == port.sender {
            let port = self.ports.remove(port_id).expect("just looked up");
            let mut all = port.pending;
            all.extend(port.receivers);
            return sorted(&all).into_iter().map(|t| Effect::Deliver(t, disconnect_envelope(port_id, None))).collect();
        }
        if !(port.pending.remove(&ctx) | port.receivers.remove(&ctx)) {
            return vec![];
        }
        if port.pending.is_empty() && port.receivers.is_empty() {
            let port = self.ports.remove(port_id).expect("just looked up");
            let error = if port.ever_accepted { None } else { Some(NO_RECEIVER) };
            return vec![Effect::Deliver(port.sender, disconnect_envelope(port_id, error))];
        }
        vec![]
    }

    // ---- One-shot messages ---------------------------------------------------------------------------------------

    /// `runtime.sendMessage` / `tabs.sendMessage`. The first recipient that answers wins; `reply` is completed from
    /// [`Bus::reply`] with `{ "response": value }`, or `{}` if listeners existed but none answered.
    pub fn send_message(&mut self, sender: ContextId, message: Value, target: Target, sender_json: Value, reply: Reply) -> Vec<Effect> {
        let extension_id = match self.sender_context(sender) {
            Ok(c) => c.extension_id.clone(),
            Err(e) => return vec![Effect::Complete(reply, Err(e))],
        };
        if self.requests.values().filter(|r| r.extension_id == extension_id).count() >= MAX_PENDING_MESSAGES_PER_EXTENSION {
            return vec![Effect::Complete(reply, Err(ApiError::runtime("Too many pending messages.")))];
        }
        let targets = self.targets(&extension_id, sender, &target);
        if targets.is_empty() {
            return vec![Effect::Complete(reply, Err(ApiError::runtime(NO_RECEIVER)))];
        }
        self.next_request += 1;
        let request_id = self.next_request;
        let effects = targets
            .iter()
            .map(|&t| Effect::Deliver(t, json!({ "type": "message", "requestId": request_id, "message": message, "sender": sender_json })))
            .collect();
        self.requests.insert(
            request_id,
            Request { extension_id, sender, waiting: targets.into_iter().collect(), any_listener: false, reply: Some(reply) },
        );
        effects
    }

    /// A recipient's answer. `has_listeners`: it had at least one `onMessage` listener. `handled`: a listener answered.
    pub fn reply(
        &mut self,
        ctx: ContextId,
        request_id: u64,
        has_listeners: bool,
        handled: bool,
        response: Value,
    ) -> Result<Vec<Effect>, ApiError> {
        // A late answer to a request that has already been settled is normal (another recipient won) and ignored.
        let Some(req) = self.requests.get_mut(&request_id) else { return Ok(vec![]) };
        if !req.waiting.remove(&ctx) {
            return Err(ApiError::new(ErrorKind::PermissionDenied, "this context was not addressed by that message"));
        }
        req.any_listener |= has_listeners;
        let mut effects = vec![];
        if handled && let Some(reply) = req.reply.take() {
            effects.push(Effect::Complete(reply, Ok(json!({ "response": response }))));
        }
        effects.extend(self.settle_if_done(request_id));
        Ok(effects)
    }

    fn settle_if_done(&mut self, request_id: u64) -> Option<Effect> {
        if !self.requests.get(&request_id)?.waiting.is_empty() {
            return None;
        }
        let req = self.requests.remove(&request_id)?;
        let reply = req.reply?;
        // Listeners existed but none answered: no `response` key, which the shim turns into `undefined` as browsers do.
        Some(Effect::Complete(reply, if req.any_listener { Ok(json!({})) } else { Err(ApiError::runtime(NO_RECEIVER)) }))
    }

    // ---- Lifecycle -------------------------------------------------------------------------------------------------

    /// A context went away (tab closed, page navigated, background shut down): close its ports and stop waiting for it.
    pub fn context_destroyed(&mut self, ctx: ContextId) -> Vec<Effect> {
        if self.contexts.remove(&ctx).is_none() {
            return vec![];
        }
        let mut effects = vec![];
        let mut port_ids: Vec<String> = self
            .ports
            .iter()
            .filter(|(_, p)| p.sender == ctx || p.pending.contains(&ctx) || p.receivers.contains(&ctx))
            .map(|(id, _)| id.clone())
            .collect();
        port_ids.sort();
        for id in port_ids {
            effects.extend(self.leave_port(ctx, &id));
        }

        let mut request_ids: Vec<u64> =
            self.requests.iter().filter(|(_, r)| r.sender == ctx || r.waiting.contains(&ctx)).map(|(id, _)| *id).collect();
        request_ids.sort_unstable();
        for id in request_ids {
            let Some(req) = self.requests.get_mut(&id) else { continue };
            if req.sender == ctx {
                // The asker is gone; free the call.
                let req = self.requests.remove(&id).expect("just looked up");
                if let Some(reply) = req.reply {
                    effects.push(Effect::Complete(reply, Err(ApiError::runtime("Extension context invalidated."))));
                }
            } else {
                req.waiting.remove(&ctx);
                effects.extend(self.settle_if_done(id));
            }
        }
        effects
    }

    /// The extension was removed: forget its contexts and ports, and fail the calls still waiting for its contexts.
    pub fn drop_extension(&mut self, extension_id: &str) -> Vec<Effect> {
        self.contexts.retain(|_, c| c.extension_id != extension_id);
        self.ports.retain(|_, p| p.extension_id != extension_id);
        let mut ids: Vec<u64> = self.requests.iter().filter(|(_, r)| r.extension_id == extension_id).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.requests.remove(&id)?.reply)
            .map(|reply| Effect::Complete(reply, Err(ApiError::runtime("Extension context invalidated."))))
            .collect()
    }

    pub fn open_port_count(&self) -> usize {
        self.ports.len()
    }

    pub fn pending_message_count(&self) -> usize {
        self.requests.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const EXT: &str = "ext";

    fn ctx(id: ContextId, kind: ContextKind, tab: Option<i64>, frame: Option<i64>) -> ContextInfo {
        ContextInfo { id, extension_id: EXT.into(), kind, tab_id: tab, frame_id: frame, url: None }
    }

    /// background = 1, popup = 2, content scripts: 10 (tab 5 top frame), 11 (tab 5 subframe 3), 20 (tab 6 top frame)
    fn bus() -> Bus {
        let mut b = Bus::new();
        for c in [
            ctx(1, ContextKind::Background, None, None),
            ctx(2, ContextKind::ExtensionPage, None, None),
            ctx(10, ContextKind::ContentScript, Some(5), Some(0)),
            ctx(11, ContextKind::ContentScript, Some(5), Some(3)),
            ctx(20, ContextKind::ContentScript, Some(6), Some(0)),
        ] {
            b.context_created(c).unwrap();
        }
        b
    }

    fn deliveries(effects: &[Effect]) -> Vec<(ContextId, String)> {
        effects
            .iter()
            .filter_map(|e| if let Effect::Deliver(c, v) = e { Some((*c, v["type"].as_str().unwrap().to_string())) } else { None })
            .collect()
    }

    type Slot = Rc<RefCell<Option<ApiResult>>>;

    fn slot() -> (Slot, Reply) {
        let s: Slot = Rc::new(RefCell::new(None));
        let s2 = s.clone();
        (s, Box::new(move |r| *s2.borrow_mut() = Some(r)))
    }

    fn run(effects: Vec<Effect>) -> Vec<(ContextId, Value)> {
        let mut out = vec![];
        for e in effects {
            match e {
                Effect::Deliver(c, v) => out.push((c, v)),
                Effect::Complete(reply, r) => reply(r),
            }
        }
        out
    }

    #[test]
    fn runtime_connect_reaches_background_and_pages_but_not_the_sender_or_content_scripts() {
        let mut b = bus();
        let fx = b.connect(10, "10.1".into(), "chat", Target::Runtime, json!({"id": EXT})).unwrap();
        assert_eq!(deliveries(&fx), [(1, "connect".into()), (2, "connect".into())]);
        let mut b = bus();
        let fx = b.connect(1, "1.1".into(), "x", Target::Runtime, json!({})).unwrap();
        assert_eq!(deliveries(&fx), [(2, "connect".into())], "the background does not connect to itself");
    }

    #[test]
    fn tabs_connect_targets_content_scripts_of_that_tab_and_optionally_one_frame() {
        let mut b = bus();
        let fx = b.connect(1, "1.1".into(), "", Target::Tab { tab_id: 5, frame_id: None }, json!({})).unwrap();
        assert_eq!(deliveries(&fx), [(10, "connect".into()), (11, "connect".into())]);
        let mut b = bus();
        let fx = b.connect(1, "1.1".into(), "", Target::Tab { tab_id: 5, frame_id: Some(3) }, json!({})).unwrap();
        assert_eq!(deliveries(&fx), [(11, "connect".into())]);
        let fx = b.connect(1, "1.2".into(), "", Target::Tab { tab_id: 99, frame_id: None }, json!({})).unwrap();
        assert_eq!(deliveries(&fx), [(1, "port_disconnect".into())], "no content script: the sender is told immediately");
    }

    #[test]
    fn port_lifecycle_messages_flow_both_ways_and_disconnect_propagates() {
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "chat", Target::Runtime, json!({})).unwrap());
        // The background has a listener, the popup does not.
        assert!(b.accept(1, "10.1", true).is_empty());
        assert!(b.accept(2, "10.1", false).is_empty());

        let to_bg = run(b.post(10, "10.1", json!("hello")).unwrap());
        assert_eq!(to_bg.len(), 1);
        assert_eq!(to_bg[0].0, 1);
        assert_eq!(to_bg[0].1["message"], "hello");
        let back = run(b.post(1, "10.1", json!({"n": 1})).unwrap());
        assert_eq!(back[0].0, 10);

        // The receiver hangs up: the sender is told, without an error because the connection had worked.
        let fx = run(b.disconnect(1, "10.1"));
        assert_eq!(fx.len(), 1);
        assert_eq!(fx[0].0, 10);
        assert_eq!(fx[0].1["error"], Value::Null);
        assert_eq!(b.open_port_count(), 0);
        assert!(b.post(10, "10.1", json!(1)).is_err(), "a closed port cannot be used");
    }

    #[test]
    fn sender_disconnect_closes_all_receivers() {
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        b.accept(1, "10.1", true);
        b.accept(2, "10.1", true);
        // Messages from the sender reach every receiver.
        assert_eq!(run(b.post(10, "10.1", json!(1)).unwrap()).len(), 2);
        let fx = run(b.disconnect(10, "10.1"));
        assert_eq!(fx.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1, 2]);
        // With two receivers, one leaving does not close the port.
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        b.accept(1, "10.1", true);
        b.accept(2, "10.1", true);
        assert!(b.disconnect(1, "10.1").is_empty());
        assert_eq!(b.open_port_count(), 1);
        assert_eq!(run(b.disconnect(2, "10.1")).len(), 1, "the last receiver leaving closes the port");
    }

    #[test]
    fn nobody_listening_fails_the_connection_with_the_standard_error() {
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        assert!(b.accept(1, "10.1", false).is_empty());
        let fx = run(b.accept(2, "10.1", false));
        assert_eq!(fx.len(), 1);
        assert_eq!(fx[0].0, 10);
        assert_eq!(fx[0].1["error"], NO_RECEIVER);
        assert_eq!(b.open_port_count(), 0);
    }

    #[test]
    fn port_ids_and_membership_are_validated() {
        let mut b = bus();
        assert!(b.connect(10, "11.1".into(), "", Target::Runtime, json!({})).is_err(), "cannot claim another context's id");
        assert!(b.connect(10, "10.1".repeat(40), "", Target::Runtime, json!({})).is_err());
        assert!(b.connect(999, "999.1".into(), "", Target::Runtime, json!({})).is_err(), "unregistered context");
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        assert!(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).is_err(), "duplicate id");
        b.accept(1, "10.1", true);
        // A context that is not an end of the port cannot post to it, or close it.
        assert_eq!(b.post(20, "10.1", json!(1)).unwrap_err().kind, ErrorKind::PermissionDenied);
        assert!(b.disconnect(20, "10.1").is_empty());
        assert_eq!(b.open_port_count(), 1);
        // Unknown ports are ignored by accept/disconnect and rejected by post.
        assert!(b.accept(1, "nope", true).is_empty() && b.disconnect(1, "nope").is_empty());
        assert!(b.post(1, "nope", json!(1)).is_err());
    }

    #[test]
    fn destroying_a_context_closes_its_ports() {
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        b.accept(1, "10.1", true);
        b.accept(2, "10.1", false);
        // The content script's tab closes: the background hears about it.
        let fx = run(b.context_destroyed(10));
        assert_eq!(fx.len(), 1);
        assert_eq!(fx[0].0, 1);
        assert_eq!(fx[0].1["type"], "port_disconnect");
        assert_eq!(b.open_port_count(), 0);
        assert!(b.context(10).is_none());

        // The background shuts down while a content script is connected to it.
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        b.accept(1, "10.1", true);
        b.accept(2, "10.1", false);
        let fx = run(b.context_destroyed(1));
        assert_eq!(fx.len(), 1);
        assert_eq!(fx[0].0, 10);
        assert_eq!(fx[0].1["error"], Value::Null);
        assert!(b.context_destroyed(1).is_empty(), "destroying twice is harmless");
    }

    #[test]
    fn send_message_first_answer_wins() {
        let mut b = bus();
        let (result, reply) = slot();
        let fx = b.send_message(10, json!("ping"), Target::Runtime, json!({"id": EXT}), reply);
        assert_eq!(deliveries(&fx), [(1, "message".into()), (2, "message".into())]);
        let request = fx.iter().find_map(|e| if let Effect::Deliver(_, v) = e { v["requestId"].as_u64() } else { None }).unwrap();
        run(fx);
        assert!(result.borrow().is_none(), "still waiting");

        // The popup answers first and wins; the background's later answer is ignored.
        run(b.reply(2, request, true, true, json!("pong")).unwrap());
        assert_eq!(result.borrow_mut().take().unwrap().unwrap(), json!({"response": "pong"}));
        assert!(run(b.reply(1, request, true, true, json!("late")).unwrap()).is_empty());
        assert_eq!(b.pending_message_count(), 0);
    }

    #[test]
    fn send_message_outcomes_when_nobody_answers() {
        // Listeners exist but none answered: resolves with nothing.
        let mut b = bus();
        let (result, reply) = slot();
        let fx = b.send_message(10, json!(1), Target::Runtime, json!({}), reply);
        let request = fx.iter().find_map(|e| if let Effect::Deliver(_, v) = e { v["requestId"].as_u64() } else { None }).unwrap();
        run(fx);
        run(b.reply(1, request, true, false, Value::Null).unwrap());
        assert!(result.borrow().is_none());
        run(b.reply(2, request, false, false, Value::Null).unwrap());
        assert_eq!(result.borrow_mut().take().unwrap().unwrap(), json!({}));

        // No listeners anywhere: the standard error.
        let (result, reply) = slot();
        let fx = b.send_message(10, json!(1), Target::Runtime, json!({}), reply);
        let request = fx.iter().find_map(|e| if let Effect::Deliver(_, v) = e { v["requestId"].as_u64() } else { None }).unwrap();
        run(fx);
        run(b.reply(1, request, false, false, Value::Null).unwrap());
        run(b.reply(2, request, false, false, Value::Null).unwrap());
        assert_eq!(result.borrow_mut().take().unwrap().unwrap_err().message, NO_RECEIVER);

        // Nobody to ask at all fails at once.
        let (result, reply) = slot();
        run(b.send_message(1, json!(1), Target::Tab { tab_id: 77, frame_id: None }, json!({}), reply));
        assert_eq!(result.borrow_mut().take().unwrap().unwrap_err().message, NO_RECEIVER);
    }

    #[test]
    fn a_recipient_dying_does_not_hang_the_sender() {
        let mut b = bus();
        let (result, reply) = slot();
        let fx = b.send_message(1, json!("hi"), Target::Tab { tab_id: 5, frame_id: None }, json!({}), reply);
        let request = fx.iter().find_map(|e| if let Effect::Deliver(_, v) = e { v["requestId"].as_u64() } else { None }).unwrap();
        run(fx);
        run(b.reply(10, request, true, false, Value::Null).unwrap());
        assert!(result.borrow().is_none());
        run(b.context_destroyed(11)); // The subframe's content script was the last one we waited for.
        assert_eq!(result.borrow_mut().take().unwrap().unwrap(), json!({}));

        // The asker dying frees its call.
        let (result, reply) = slot();
        run(b.send_message(10, json!("hi"), Target::Runtime, json!({}), reply));
        run(b.context_destroyed(10));
        assert!(result.borrow_mut().take().unwrap().is_err());
        assert_eq!(b.pending_message_count(), 0);
    }

    #[test]
    fn only_addressed_contexts_may_answer() {
        let mut b = bus();
        let (_result, reply) = slot();
        let fx = b.send_message(10, json!(1), Target::Runtime, json!({}), reply);
        let request = fx.iter().find_map(|e| if let Effect::Deliver(_, v) = e { v["requestId"].as_u64() } else { None }).unwrap();
        run(fx);
        assert_eq!(b.reply(20, request, true, true, json!("forged")).unwrap_err().kind, ErrorKind::PermissionDenied);
        // Unknown requests are ignored, not errors (they may have just been settled).
        assert!(b.reply(1, 9999, true, true, json!(1)).unwrap().is_empty());
    }

    #[test]
    fn contexts_of_other_extensions_are_isolated() {
        let mut b = bus();
        b.context_created(ContextInfo {
            id: 100,
            extension_id: "other".into(),
            kind: ContextKind::Background,
            tab_id: None,
            frame_id: None,
            url: None,
        })
        .unwrap();
        let fx = b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap();
        assert!(deliveries(&fx).iter().all(|(c, _)| *c != 100));
        let (result, reply) = slot();
        let fx = b.send_message(10, json!(1), Target::Runtime, json!({}), reply);
        assert!(deliveries(&fx).iter().all(|(c, _)| *c != 100));
        drop(result);
        assert!(b.context_created(ctx(1, ContextKind::Background, None, None)).is_err(), "duplicate context id");
    }

    #[test]
    fn dropping_an_extension_fails_its_pending_calls_and_forgets_everything() {
        let mut b = bus();
        run(b.connect(10, "10.1".into(), "", Target::Runtime, json!({})).unwrap());
        let (result, reply) = slot();
        run(b.send_message(10, json!(1), Target::Runtime, json!({}), reply));
        run(b.drop_extension(EXT));
        assert!(result.borrow_mut().take().unwrap().is_err());
        assert_eq!((b.open_port_count(), b.pending_message_count(), b.context(1).is_none()), (0, 0, true));
        assert_ne!(b.fresh_port_id(7), b.fresh_port_id(7));
    }

    #[test]
    fn limits_protect_against_runaway_extensions() {
        let mut b = bus();
        let mut refused = 0;
        for i in 0..(MAX_PORTS_PER_EXTENSION + 5) {
            if b.connect(10, format!("10.{i}"), "", Target::Runtime, json!({})).is_err() {
                refused += 1;
            }
        }
        assert_eq!(refused, 5);
        assert_eq!(b.open_port_count(), MAX_PORTS_PER_EXTENSION);
    }
}
