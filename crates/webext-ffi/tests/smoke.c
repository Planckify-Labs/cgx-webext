/* Exercises the C ABI the way a browser would. Run by tests/c_abi.rs. */
#include "webext.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(cond)                                                                     \
    do {                                                                                \
        if (!(cond)) {                                                                  \
            fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond);    \
            exit(1);                                                                    \
        }                                                                               \
    } while (0)

#define S(literal) ((WebExtStr) { (const uint8_t*)(literal), sizeof(literal) - 1 })

typedef struct Host {
    char stored[4096];
    char last_event[256];
    int event_count;
    uint64_t pending_browser_token;
    uint64_t pending_permission_token;
    int64_t wakeup;
    char deliveries[8][512];
    uint64_t delivery_ctx[8];
    int delivery_count;
} Host;

static WebExtString host_read_storage(void* u, WebExtStr id, WebExtStr key)
{
    (void)u; (void)id; (void)key;
    WebExtString none = { NULL, 0, 0 };
    return none;
}
static void host_write_storage(void* u, WebExtStr id, WebExtStr key, WebExtStr json)
{
    Host* h = u;
    (void)id; (void)key;
    snprintf(h->stored, sizeof h->stored, "%.*s", (int)json.len, (const char*)json.ptr);
}
static int64_t host_now(void* u) { (void)u; return 1000; }
static WebExtString host_locale(void* u) { (void)u; return webext_string_from((const uint8_t*)"en-US", 5); }
static void host_event(void* u, WebExtStr id, WebExtStr event, WebExtStr args)
{
    Host* h = u;
    (void)id;
    h->event_count++;
    snprintf(h->last_event, sizeof h->last_event, "%.*s %.*s", (int)event.len, (const char*)event.ptr, (int)args.len, (const char*)args.ptr);
}
static void host_call_browser(void* u, WebExtStr call, uint64_t token)
{
    Host* h = u;
    (void)call;
    h->pending_browser_token = token; /* Answered later, asynchronously. */
}
static void host_request_permissions(void* u, WebExtStr id, WebExtStr req, uint64_t token)
{
    Host* h = u;
    (void)id; (void)req;
    h->pending_permission_token = token;
}
static void host_schedule(void* u, int64_t at) { ((Host*)u)->wakeup = at; }
static void host_deliver(void* u, uint64_t ctx, WebExtStr json)
{
    Host* h = u;
    int i = h->delivery_count++ % 8;
    h->delivery_ctx[i] = ctx;
    snprintf(h->deliveries[i], sizeof h->deliveries[i], "%.*s", (int)json.len, (const char*)json.ptr);
}
static const char* last_delivery(Host* h) { return h->deliveries[(h->delivery_count - 1) % 8]; }
static uint64_t last_delivery_ctx(Host* h) { return h->delivery_ctx[(h->delivery_count - 1) % 8]; }
static WebExtString host_describe_tab(void* u, int64_t tab)
{
    (void)u;
    char buf[64];
    int n = snprintf(buf, sizeof buf, "{\"id\":%lld,\"title\":\"Tab\"}", (long long)tab);
    return webext_string_from((const uint8_t*)buf, (size_t)n);
}

typedef struct Reply {
    int called;
    bool ok;
    char json[1024];
} Reply;

static void on_reply(void* u, bool ok, WebExtStr json)
{
    Reply* r = u;
    r->called++;
    r->ok = ok;
    snprintf(r->json, sizeof r->json, "%.*s", (int)json.len, (const char*)json.ptr);
}

static void call(WebExtHandle* h, Reply* r, const char* json)
{
    memset(r, 0, sizeof *r);
    WebExtStr s = { (const uint8_t*)json, strlen(json) };
    webext_dispatch(h, s, on_reply, r);
}

static char* slurp(const char* path, size_t* len)
{
    FILE* f = fopen(path, "rb");
    CHECK(f);
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    char* buf = malloc((size_t)n);
    CHECK(fread(buf, 1, (size_t)n, f) == (size_t)n);
    fclose(f);
    *len = (size_t)n;
    return buf;
}

int main(int argc, char** argv)
{
    CHECK(argc == 3);   /* package.zip and the same extension unpacked in a directory */
    CHECK(webext_abi_version() == WEBEXT_ABI_VERSION);

    Host host;
    memset(&host, 0, sizeof host);
    WebExtHostCallbacks cb = {
        .user_data = &host,
        .read_storage = host_read_storage,
        .write_storage = host_write_storage,
        .now_ms = host_now,
        .ui_locale = host_locale,
        .dispatch_event = host_event,
        .call_browser = host_call_browser,
        .request_permissions = host_request_permissions,
        .schedule_alarm_check = host_schedule,
        .deliver_to_context = host_deliver,
        .describe_tab = host_describe_tab,
    };
    WebExtHandle* h = webext_create(&cb);
    CHECK(h);

    size_t pkg_len;
    char* pkg = slurp(argv[1], &pkg_len);

    /* Bad package -> structured error. */
    WebExtString bad = webext_install_package(h, (const uint8_t*)"nope", 4, S(""), S("chrome-extension://x/"));
    CHECK(bad.ptr && strstr((char*)bad.ptr, "\"ok\":false"));
    webext_string_free(bad);

    /* An empty base URL is refused ("auto" is the way to let the library choose). */
    WebExtString noid = webext_install_package(h, (const uint8_t*)pkg, pkg_len, S(""), S(""));
    CHECK(noid.ptr && strstr((char*)noid.ptr, "base_url"));
    webext_string_free(noid);

    WebExtString res = webext_install_package(h, (const uint8_t*)pkg, pkg_len, S("testextid"), S("chrome-extension://testextid/"));
    CHECK(res.ptr && strstr((char*)res.ptr, "\"ok\":true"));
    CHECK(strstr((char*)res.ptr, "\"displayName\":\"C ABI Test\""));
    webext_string_free(res);
    free(pkg);

    /* Synchronous core API: storage round trip, event + persistence callbacks. */
    Reply r;
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"storage.local.set\",\"args\":[{\"k\":42}]}");
    CHECK(r.called == 1 && r.ok);
    /* storage.onChanged fires, then the area's own storage.local.onChanged. */
    CHECK(strstr(host.last_event, "storage.local.onChanged") && strstr(host.last_event, "\"newValue\":42"));
    CHECK(strstr(host.stored, "\"k\":42"));
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"storage.local.get\",\"args\":[\"k\"]}");
    CHECK(r.called == 1 && r.ok && strcmp(r.json, "{\"k\":42}") == 0);

    /* Errors are reported, not crashes. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"downloads.download\",\"args\":[]}");
    CHECK(r.called == 1 && !r.ok && strstr(r.json, "NotAvailable"));
    call(h, &r, "{not json");
    CHECK(r.called == 1 && !r.ok && strstr(r.json, "InvalidArguments"));
    call(h, &r, "{\"extensionId\":\"ghost\",\"function\":\"storage.local.get\"}");
    CHECK(r.called == 1 && !r.ok);

    /* Delegated call is answered asynchronously by the host. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"cookies.getAll\",\"args\":[{}]}");
    CHECK(r.called == 0 && host.pending_browser_token != 0);
    webext_complete(h, host.pending_browser_token, true, S("[{\"name\":\"c\"}]"));
    CHECK(r.called == 1 && r.ok && strcmp(r.json, "[{\"name\":\"c\"}]") == 0);

    /* ...or fails. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"cookies.getAll\",\"args\":[{}]}");
    webext_complete(h, host.pending_browser_token, false, S("No such tab"));
    CHECK(r.called == 1 && !r.ok && strstr(r.json, "No such tab"));

    /* The browser reports its tabs; the core answers tabs.query from them and fires tabs.onCreated. */
    WebExtString state = webext_host_event(h, S("browserState"),
        S("{\"windows\":[{\"id\":1,\"focused\":true,\"tabs\":[{\"id\":7,\"url\":\"https://example.com/\",\"active\":true}]}]}"));
    CHECK(state.ptr && strcmp((const char*)state.ptr, "null") == 0);
    webext_string_free(state);
    CHECK(strstr(host.last_event, "tabs.onCreated") || strstr(host.last_event, "tabs.onActivated") || strstr(host.last_event, "windows.on"));
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"tabs.query\",\"args\":[{\"active\":true}]}");
    CHECK(r.called == 1 && r.ok && strstr(r.json, "\"id\":7") && strstr(r.json, "https://example.com/"));
    WebExtString bad_event = webext_host_event(h, S("browserState"), S("{not json"));
    CHECK(bad_event.ptr && strstr((const char*)bad_event.ptr, "error"));
    webext_string_free(bad_event);

    /* Optional permission prompt answered later. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"permissions.request\",\"args\":[{\"permissions\":[\"history\"]}]}");
    CHECK(r.called == 0 && host.pending_permission_token != 0);
    webext_complete_permission(h, host.pending_permission_token, true);
    CHECK(r.called == 1 && r.ok && strcmp(r.json, "true") == 0);

    /* Alarms: schedule hint, then tick. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"alarms.create\",\"args\":[\"a\",{\"when\":500}]}");
    CHECK(r.ok && host.wakeup == 500);
    int before = host.event_count;
    webext_tick(h);
    CHECK(host.event_count == before + 1 && strstr(host.last_event, "alarms.onAlarm"));

    /* Shim init data, content scripts, resources. */
    WebExtString init = webext_init_data(h, S("testextid"));
    CHECK(init.ptr && strstr((char*)init.ptr, "\"surface\"") && strstr((char*)init.ptr, "\"syncFunctions\""));
    webext_string_free(init);
    WebExtString info = webext_extension_info(h, S("testextid"));
    CHECK(info.ptr && strstr((char*)info.ptr, "\"serviceWorker\":\"bg.js\"") && strstr((char*)info.ptr, "\"csp\""));
    webext_string_free(info);
    WebExtString no_info = webext_extension_info(h, S("ghost"));
    CHECK(no_info.ptr == NULL);
    WebExtString unknown = webext_init_data(h, S("ghost"));
    CHECK(unknown.ptr == NULL);

    WebExtString cs = webext_content_scripts(h, S("testextid"), S("https://example.com/page"), true, S(""));
    CHECK(cs.ptr && strstr((char*)cs.ptr, "\"js\":[\"content.js\"]"));
    webext_string_free(cs);
    cs = webext_content_scripts(h, S("testextid"), S("https://other.org/"), true, S(""));
    CHECK(cs.ptr && strcmp((char*)cs.ptr, "[]") == 0);
    webext_string_free(cs);

    CHECK(webext_is_web_accessible(h, S("testextid"), S("public/logo.png"), 1, S("https://example.com/")));
    CHECK(!webext_is_web_accessible(h, S("testextid"), S("content.js"), 1, S("https://example.com/")));
    CHECK(webext_is_web_accessible(h, S("testextid"), S("content.js"), 0, S("")));

    WebExtString file = { NULL, 0, 0 };
    CHECK(webext_read_file(h, S("testextid"), S("content.js"), &file));
    CHECK(file.len > 0 && memcmp(file.ptr, "// content", 10) == 0);
    webext_string_free(file);
    CHECK(!webext_read_file(h, S("testextid"), S("../manifest.json"), &file));

    /* Messaging: contexts, ports and one-shot messages across the C boundary. */
    CHECK(!webext_context_created(h, S("not json")));
    CHECK(!webext_context_created(h, S("{\"id\":1,\"extensionId\":\"ghost\",\"kind\":\"background\"}")));
    CHECK(!webext_context_created(h, S("{\"id\":1,\"extensionId\":\"testextid\",\"kind\":\"bogus\"}")));
    CHECK(webext_context_created(h, S("{\"id\":1,\"extensionId\":\"testextid\",\"kind\":\"background\"}")));
    CHECK(webext_context_created(h, S("{\"id\":10,\"extensionId\":\"testextid\",\"kind\":\"content_script\",\"tabId\":5,\"frameId\":0,\"url\":\"https://example.com/\"}")));
    CHECK(!webext_context_created(h, S("{\"id\":10,\"extensionId\":\"testextid\",\"kind\":\"background\"}")));

    WebExtString cs_init = webext_init_data_for_context(h, 10);
    CHECK(cs_init.ptr && strstr((char*)cs_init.ptr, "\"contextKind\":\"content_script\"") && strstr((char*)cs_init.ptr, "\"contextId\":10"));
    /* The manifest in the init data still lists "alarms" as a permission; what must be absent is the API namespace. */
    CHECK(!strstr((char*)cs_init.ptr, "\"tabs\":{") && !strstr((char*)cs_init.ptr, "\"alarms\":{") && strstr((char*)cs_init.ptr, "\"runtime\":{"));
    webext_string_free(cs_init);
    WebExtString no_ctx = webext_init_data_for_context(h, 999);
    CHECK(no_ctx.ptr == NULL);

    const char* cs_connect = "{\"extensionId\":\"testextid\",\"function\":\"runtime.connect\",\"args\":[null,{\"name\":\"chat\"},\"10.1\"],\"caller\":{\"context\":\"content_script\",\"contextId\":10}}";
    call(h, &r, cs_connect);
    CHECK(r.called == 1 && r.ok && strstr(r.json, "10.1"));
    CHECK(last_delivery_ctx(&host) == 1 && strstr(last_delivery(&host), "\"type\":\"connect\"") && strstr(last_delivery(&host), "\"name\":\"chat\""));
    CHECK(strstr(last_delivery(&host), "\"title\":\"Tab\""));   /* describe_tab enriched MessageSender.tab */

    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"__bus.accept\",\"args\":[\"10.1\",true],\"caller\":{\"context\":\"background\",\"contextId\":1}}");
    CHECK(r.called == 1 && r.ok);
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"__bus.post\",\"args\":[\"10.1\",{\"hi\":1}],\"caller\":{\"context\":\"content_script\",\"contextId\":10}}");
    CHECK(r.ok && last_delivery_ctx(&host) == 1 && strstr(last_delivery(&host), "\"type\":\"port_message\"") && strstr(last_delivery(&host), "\"hi\":1"));

    /* A one-shot message is answered later by another context. */
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"runtime.sendMessage\",\"args\":[\"ping\"],\"caller\":{\"context\":\"content_script\",\"contextId\":10}}");
    CHECK(r.called == 0);
    const char* req = strstr(last_delivery(&host), "\"requestId\":");
    CHECK(req && strstr(last_delivery(&host), "\"type\":\"message\""));
    long request_id = strtol(req + strlen("\"requestId\":"), NULL, 10);
    char answer[256];
    snprintf(answer, sizeof answer, "{\"extensionId\":\"testextid\",\"function\":\"__bus.reply\",\"args\":[%ld,true,true,\"pong\"],\"caller\":{\"context\":\"background\",\"contextId\":1}}", request_id);
    Reply answer_reply;
    call(h, &answer_reply, answer);
    CHECK(answer_reply.ok);
    CHECK(r.called == 1 && r.ok && strcmp(r.json, "{\"response\":\"pong\"}") == 0);

    /* A context that disappears closes its ports. */
    webext_context_destroyed(h, 10);
    CHECK(last_delivery_ctx(&host) == 1 && strstr(last_delivery(&host), "\"type\":\"port_disconnect\""));
    webext_context_destroyed(h, 10);   /* harmless twice */
    webext_context_destroyed(h, 424242);

    /* Unpacked install with an automatic origin, and serving. */
    {
        size_t dir_len = strlen(argv[2]);
        WebExtStr dir = { (const uint8_t*)argv[2], dir_len };
        WebExtString inst = webext_install_directory(h, dir, S(""), S("auto"));
        CHECK(inst.ptr && strstr((char*)inst.ptr, "\"ok\":true") && strstr((char*)inst.ptr, "\"idSource\":\"derived\""));
        const char* base_key = strstr((char*)inst.ptr, "\"baseUrl\":\"");
        CHECK(base_key);
        base_key += strlen("\"baseUrl\":\"");
        char base[128];
        const char* base_end = strchr(base_key, '"');
        CHECK(base_end && (size_t)(base_end - base_key) < sizeof base - 64);
        memcpy(base, base_key, (size_t)(base_end - base_key));
        base[base_end - base_key] = 0;
        CHECK(strncmp(base, "chrome-extension://", 19) == 0 && base[strlen(base) - 1] == '/');
        webext_string_free(inst);

        char url[256];
        WebExtString body = { NULL, 0, 0 };

        /* A page of the extension itself: served with the shim and bootstrap inserted, and the extension CSP. */
        snprintf(url, sizeof url, "%spage.html", base);
        WebExtString meta = webext_serve(h, (WebExtStr){ (const uint8_t*)url, strlen(url) }, S(""), &body);
        CHECK(meta.ptr && strstr((char*)meta.ptr, "\"status\":200") && strstr((char*)meta.ptr, "text/html") && strstr((char*)meta.ptr, "script-src"));
        CHECK(body.ptr && strstr((char*)body.ptr, "/__cgx__/boot.js") && strstr((char*)body.ptr, "<body>x</body>"));
        webext_string_free(meta);
        webext_string_free(body);

        /* A web page may only load what web_accessible_resources allows. */
        snprintf(url, sizeof url, "%spublic/logo.png", base);
        body = (WebExtString){ NULL, 0, 0 };
        meta = webext_serve(h, (WebExtStr){ (const uint8_t*)url, strlen(url) }, S("https://example.com"), &body);
        CHECK(meta.ptr && strstr((char*)meta.ptr, "\"status\":200") && body.len == 3);
        webext_string_free(meta);
        webext_string_free(body);

        snprintf(url, sizeof url, "%scontent.js", base);
        body = (WebExtString){ NULL, 0, 0 };
        meta = webext_serve(h, (WebExtStr){ (const uint8_t*)url, strlen(url) }, S("https://example.com"), &body);
        CHECK(meta.ptr && strstr((char*)meta.ptr, "\"status\":403"));
        webext_string_free(meta);
        webext_string_free(body);

        /* Not an extension URL at all. */
        body = (WebExtString){ NULL, 0, 0 };
        meta = webext_serve(h, S("https://example.com/x"), S(""), &body);
        CHECK(meta.ptr == NULL);

        snprintf(url, sizeof url, "%scontent.js", base);
        WebExtString owner = webext_extension_for_url(h, (WebExtStr){ (const uint8_t*)url, strlen(url) });
        CHECK(owner.ptr && owner.len == 32);
        webext_string_free(owner);
    }

    /* Lifecycle. */
    webext_notify_installed(h, S("testextid"), S("install"), S(""));
    CHECK(strstr(host.last_event, "runtime.onInstalled"));
    CHECK(webext_uninstall(h, S("testextid")));
    CHECK(!webext_uninstall(h, S("testextid")));
    call(h, &r, "{\"extensionId\":\"testextid\",\"function\":\"storage.local.get\"}");
    CHECK(r.called == 1 && !r.ok);

    webext_destroy(h);
    printf("OK\n");
    return 0;
}
