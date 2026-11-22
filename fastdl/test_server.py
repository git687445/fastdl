#!/usr/bin/env python3
"""支持 HTTP Range 的本地测试服务器（模拟支持分段的下载源）"""
import http.server, socketserver, os, threading

PORT = 18466
ROOT = os.path.dirname(os.path.abspath(__file__))

class RangeHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _serve(self):
        path = self.path.lstrip("/")
        fp = os.path.join(ROOT, path)
        if not os.path.isfile(fp):
            self.send_response(404); self.end_headers(); return
        size = os.path.getsize(fp)
        rng = self.headers.get("Range")
        if rng:
            # 只支持 "bytes=start-end"
            spec = rng.split("=", 1)[1]
            start_s, end_s = spec.split("-", 1)
            start = int(start_s)
            end = int(end_s) if end_s else size - 1
            end = min(end, size - 1)
            length = end - start + 1
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            self.send_header("Content-Length", str(length))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            with open(fp, "rb") as f:
                f.seek(start)
                remaining = length
                while remaining > 0:
                    chunk = f.read(min(65536, remaining))
                    if not chunk: break
                    self.wfile.write(chunk)
                    remaining -= len(chunk)
        else:
            self.send_response(200)
            self.send_header("Content-Length", str(size))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            with open(fp, "rb") as f:
                while True:
                    chunk = f.read(65536)
                    if not chunk: break
                    self.wfile.write(chunk)

    do_GET = _serve
    do_HEAD = _serve
    def log_message(self, *a): pass

class ThreadingTCPServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True

if __name__ == "__main__":
    srv = ThreadingTCPServer(("127.0.0.1", PORT), RangeHandler)
    print(f"range server on :{PORT}", flush=True)
    srv.serve_forever()
