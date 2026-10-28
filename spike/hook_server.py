#!/usr/bin/env python3
"""Logs every hook POST (path, time, body) to $LOG and answers by $MODE.
MODE: allow | deny | allow-session | sleep:<secs> (then allow) | silent:<secs> (hold, then close)"""
import json, os, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG, MODE = os.environ["LOG"], os.environ.get("MODE", "allow")

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        t0 = time.time()
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": round(t0, 2), "path": self.path, "event": body.get("hook_event_name"), "body": body}) + "\n")
        out = {}
        if body.get("hook_event_name") == "PermissionRequest":
            m = MODE
            if m.startswith("sleep:") or m.startswith("silent:"):
                time.sleep(float(m.split(":")[1]))
                m = "allow" if m.startswith("sleep:") else None
            if m == "deny":
                d = {"behavior": "deny", "message": "REASON-XYZZY: blocked by drover spike"}
            elif m == "allow":
                d = {"behavior": "allow"}
            elif m == "allow-session":
                d = {"behavior": "allow", "updatedPermissions": [{"type": "addRules", "rules": [{"toolName": body["tool_name"], "ruleContent": body["tool_input"].get("command", "")}], "behavior": "allow", "destination": "session"}]}
            else:
                d = None
            if d:
                out = {"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": d}}
        data = json.dumps(out).encode()
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": round(time.time(), 2), "reply_after": round(time.time() - t0, 2), "reply": out}) + "\n")
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)
    def log_message(self, *a): pass

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
