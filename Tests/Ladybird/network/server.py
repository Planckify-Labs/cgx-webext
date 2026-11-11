"""Serves ./site and logs every request with the headers the test cares about."""
import http.server, sys, json

class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=sys.argv[2], **k)
    def end_headers(self):
        self.send_header('Set-Cookie', 'tracker=1')
        self.send_header('X-Server', 'test')
        self.send_header('Cache-Control', 'no-store')
        super().end_headers()
    def do_GET(self):
        with open(sys.argv[3], 'a') as log:
            log.write(json.dumps({'path': self.path, 'x-test': self.headers.get('X-Test'), 'x-added': self.headers.get('X-Added'),
                                  'referer': self.headers.get('Referer'), 'ua': self.headers.get('User-Agent')}) + '\n')
        if self.path.startswith('/report'):
            self.send_response(204); self.end_headers(); return
        super().do_GET()
    def log_message(self, *a): pass

class Server(http.server.ThreadingHTTPServer):
    # A page's requests arrive in a burst; the default backlog of 5 drops connections.
    request_queue_size = 512

Server(('127.0.0.1', int(sys.argv[1])), Handler).serve_forever()
