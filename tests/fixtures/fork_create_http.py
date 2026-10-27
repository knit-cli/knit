#!/usr/bin/env python3
"""Loopback-only native transport around the same fail-closed fork API fixture."""
import http.server
import json
import os
import pathlib
import subprocess
import sys
import time

root = pathlib.Path(os.environ['GH_FAKE_DIR'])
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def handle_api(self):
        with (root / 'headers').open('a') as stream:
            stream.write(json.dumps({'authorization': self.headers.get('Authorization')}) + '\n')
        settings = json.loads((root / 'settings.json').read_text())
        if settings.get('hang_ready') and (root / 'created').exists() and self.path == '/repos/contributor/library':
            with (root / 'calls').open('a') as stream:
                stream.write(json.dumps({'method': self.command, 'endpoint': self.path.lstrip('/'), 'payload': {}}) + '\n')
            (root / 'sleep-started').write_text(str(os.getpid()))
            time.sleep(30)
            (root / 'sleep-completed').write_text('unexpected')
        if self.headers.get('Authorization') != 'Bearer synthetic-upstream-token':
            status, body = 401, b'{"message":"wrong credential binding"}'
        else:
            args = [sys.executable, sys.argv[1], 'api', self.path.lstrip('/'), '--method', self.command]
            payload = None
            if self.command == 'POST':
                args += ['--input', '-']
                payload = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            result = subprocess.run(args, input=payload, capture_output=True)
            status = (202 if self.command == 'POST' else 200) if result.returncode == 0 else 404 if b'HTTP 404' in result.stderr else 500
            body = result.stdout if result.returncode == 0 else json.dumps({'message': result.stderr.decode()}).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = handle_api
    do_POST = handle_api

server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
print('http://127.0.0.1:' + str(server.server_port), flush=True)
server.serve_forever()
