"""Serves ./site for the isolated-world test and logs every request, so that the pages and the content scripts can report
their results by requesting /report?c=<check>&ok=<0|1>&d=<detail>."""
import http.server, json, sys, time

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=sys.argv[2], **k)

    def end_headers(self):
        self.send_header('Cache-Control', 'no-store')
        # A page that forbids eval and requires Trusted Types, as strict sites do.
        if self.path.startswith('/csp.html'):
            self.send_header('Content-Security-Policy', "script-src 'self' 'unsafe-inline'; require-trusted-types-for 'script'")
        super().end_headers()

    def do_GET(self):
        with open(sys.argv[3], 'a') as log:
            log.write(json.dumps({'path': self.path}) + '\n')
        if self.path.startswith('/report'):
            self.send_response(204)
            self.end_headers()
            return
        if self.path.startswith('/slow'):
            # A request that is still under way when the page navigates away.
            time.sleep(2.5)
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            try:
                self.wfile.write(b'{}')
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        super().do_GET()

    def log_message(self, *a):
        pass

class Server(http.server.ThreadingHTTPServer):
    request_queue_size = 512

Server(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
