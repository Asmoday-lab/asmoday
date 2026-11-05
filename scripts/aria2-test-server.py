#!/usr/bin/env python3
"""Range 支持 + 限速的测试文件服务器（aria2 冒烟专用）。

python -m http.server 不支持 Range，也跑得太快，无法观测分段 / 暂停 / 续传。
本脚本：HEAD 声明 Accept-Ranges，GET 支持单区间 Range，按 --rate 限速推流。

    python scripts/aria2-test-server.py --dir target/smoke-aria2/webroot \
        --port 4978 --rate 1048576
"""

import argparse
import os
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class RangeHandler(BaseHTTPRequestHandler):
    root = "."
    rate = 0

    def log_message(self, *args):
        pass

    def _path(self):
        rel = self.path.split("?")[0].lstrip("/")
        return os.path.join(self.root, rel)

    def _size(self):
        try:
            return os.path.getsize(self._path())
        except OSError:
            return None

    def do_HEAD(self):
        size = self._size()
        if size is None:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", f'"{size}-test"')
        self.end_headers()

    def do_GET(self):
        path = self._path()
        size = self._size()
        if size is None:
            self.send_error(404)
            return
        start, end = 0, size - 1
        status = 200
        header = self.headers.get("Range", "")
        match = re.match(r"bytes=(\d+)-(\d*)", header)
        if match:
            start = int(match.group(1))
            if match.group(2):
                end = min(int(match.group(2)), size - 1)
            if start >= size:
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{size}")
                self.end_headers()
                return
            status = 206
        length = end - start + 1
        self.send_response(status)
        self.send_header("Content-Length", str(length))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", f'"{size}-test"')
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()

        chunk = 64 * 1024
        sent = 0
        with open(path, "rb") as handle:
            handle.seek(start)
            while sent < length:
                data = handle.read(min(chunk, length - sent))
                if not data:
                    break
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    return
                sent += len(data)
                if self.rate > 0:
                    time.sleep(len(data) / self.rate)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", required=True)
    parser.add_argument("--port", type=int, default=4978)
    parser.add_argument("--rate", type=int, default=1024 * 1024, help="bytes/sec, 0 = unlimited")
    args = parser.parse_args()
    RangeHandler.root = args.dir
    RangeHandler.rate = args.rate
    server = ThreadingHTTPServer(("127.0.0.1", args.port), RangeHandler)
    print(f"serving {args.dir} on 127.0.0.1:{args.port} (rate={args.rate})", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
