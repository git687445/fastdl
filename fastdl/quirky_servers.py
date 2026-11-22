#!/usr/bin/env python3
"""两个"不规矩"的 HTTP 服务器，验证 fastdl 的探测回退与未知大小降级。

A (18471): Range 请求回 206 但**不带 Content-Range**（Content-Length 只报片段长度）；
            普通 GET 回 200 + 全量 Content-Length → 探测回退应拿到 total，走分段下载。
B (18472): 普通 GET 无 Content-Length（分块流式）；Range 请求也回 200（忽略 Range，
            不带长度）→ total 未知 → 引擎应降级单流下载并成功写全。
内容均为确定性字节 pat(i) = (i*7 + i//65536) & 0xFF，各 1MB。
"""
import http.server
import socketserver
import sys

N = 1 << 20  # 1MB


def byte_at(i: int) -> int:
    return (i * 7 + i // 65536) & 0xFF


class RangeHandler(http.server.BaseHTTPRequestHandler):
    """A: 206 无 Content-Range；普通 GET 有全量 CL"""

    def log_message(self, *a):
        pass

    def do_GET(self):
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            spec = rng[6:]
            start_s, _, end_s = spec.partition("-")
            start = int(start_s) if start_s else 0
            end = int(end_s) if end_s else N - 1
            end = min(end, N - 1)
            body_len = end - start + 1
            self.send_response(206)
            # 故意不发 Content-Range
            self.send_header("Content-Length", str(body_len))
            self.end_headers()
            self.wfile.write(bytes(byte_at(i) for i in range(start, end + 1)))
        else:
            self.send_response(200)
            self.send_header("Content-Length", str(N))
            self.end_headers()
            self.wfile.write(bytes(byte_at(i) for i in range(N)))


class NoInfoHandler(http.server.BaseHTTPRequestHandler):
    """B: 无 Content-Length（HTTP/1.0 无 CL → 连接关闭即 EOF）；Range 被忽略回 200 全量"""

    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        # 不报长度、不用 chunked（HTTP/1.0 下 chunked 是非法的，hyper 会拒）：
        # 流式写到连接关闭 = 客户端读到的"未知大小"
        for off in range(0, N, 4096):
            chunk = bytes(byte_at(i) for i in range(off, min(off + 4096, N)))
            self.wfile.write(chunk)


def serve(handler_cls, port):
    with socketserver.ThreadingTCPServer(("127.0.0.1", port), handler_cls) as httpd:
        httpd.daemon_threads = True
        print(f"server on 127.0.0.1:{port}", flush=True)
        httpd.serve_forever()


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "A"
    serve(RangeHandler, 18471) if which == "A" else serve(NoInfoHandler, 18472)
