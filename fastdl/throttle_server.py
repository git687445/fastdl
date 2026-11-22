#!/usr/bin/env python3
"""限速版 Range 服务器（演示用）：服务 bench.bin，全局带宽限 6MB/s。
支持 HTTP Range（206 + Content-Range），让 GUI 的暂停/继续可被肉眼观察到。"""
import http.server
import os
import socket
import threading
import time

ROOT = os.path.dirname(os.path.abspath(__file__))
FILE = os.path.join(ROOT, "bench.bin")
LIMIT_BYTES = 6 * 1024 * 1024  # 全局 6MB/s
CHUNK = 64 * 1024

class TokenBucket:
    def __init__(self, rate, cap):
        self.rate = rate
        self.tokens = cap
        self.lock = threading.Lock()
        self.last = time.monotonic()

    def take(self, n):
        while True:
            with self.lock:
                now = time.monotonic()
                self.tokens = min(cap_limit, self.tokens + (now - self.last) * rate_limit)
                self.last = now
                if self.tokens >= n:
                    self.tokens -= n
                    return
                need = n - self.tokens
                sleep_for = need / rate_limit
            time.sleep(min(sleep_for, 0.05))

rate_limit = LIMIT_BYTES
cap_limit = LIMIT_BYTES  # 首 6MB 即满桶，随后恒定 6MB/s
bucket = TokenBucket(rate_limit, cap_limit)


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/bench.bin" and os.path.isfile(FILE):
            size = os.path.getsize(FILE)
            rng = self.headers.get("Range")
            start, end = 0, size - 1
            status = 200
            if rng and rng.startswith("bytes="):
                s, _, e = rng[6:].partition("-")
                start = int(s) if s else 0
                end = int(e) if e else size - 1
                end = min(end, size - 1)
                status = 206
                length = end - start + 1
                self.send_response(status)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Length", str(length))
                self.send_header("ETag", '"bench-demo"')
                self.end_headers()
                with open(FILE, "rb") as f:
                    f.seek(start)
                    remaining = length
                    while remaining > 0:
                        n = min(CHUNK, remaining)
                        bucket.take(n)
                        data = f.read(n)
                        if not data:
                            break
                        try:
                            self.wfile.write(data)
                            self.wfile.flush()
                        except (BrokenPipeError, ConnectionResetError):
                            return
                        remaining -= len(data)
            else:
                self.send_response(200)
                self.send_header("Content-Length", str(size))
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("ETag", '"bench-demo"')
                self.end_headers()
                with open(FILE, "rb") as f:
                    remaining = size
                    while remaining > 0:
                        n = min(CHUNK, remaining)
                        bucket.take(n)
                        data = f.read(n)
                        if not data:
                            break
                        try:
                            self.wfile.write(data)
                            self.wfile.flush()
                        except (BrokenPipeError, ConnectionResetError):
                            return
                        remaining -= len(data)
        else:
            self.send_response(404)
            self.end_headers()


if __name__ == "__main__":
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 18468), H)
    print(f"throttle Range server on 127.0.0.1:18468 (limit {LIMIT_BYTES//1048576}MB/s)")
    srv.serve_forever()
