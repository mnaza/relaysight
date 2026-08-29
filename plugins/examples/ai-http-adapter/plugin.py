#!/usr/bin/env python3
import json
import os
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PLUGIN_ID = os.getenv("PLUGIN_ID", "ai-http-adapter")
PLUGIN_TOKEN = os.getenv("PLUGIN_TOKEN", "")
UPSTREAM_URL = os.getenv("UPSTREAM_AI_URL", "")
UPSTREAM_TOKEN = os.getenv("UPSTREAM_AI_TOKEN", "")
PORT = int(os.getenv("PORT", "9001"))
SIMULATED_AI = os.getenv("SIMULATED_AI", "false").lower() in {"1", "true", "yes", "on"}

MANIFEST = {
    "id": PLUGIN_ID,
    "name": os.getenv("PLUGIN_NAME", "AI HTTP Adapter"),
    "version": "0.2.0",
    "protocol_version": 1,
    "vendor": os.getenv("PLUGIN_VENDOR", "example"),
    "description": "Adapter for a custom AI inference HTTP endpoint.",
    "capabilities": ["ai_analyze"],
}

class Handler(BaseHTTPRequestHandler):
    def _auth(self):
        if not PLUGIN_TOKEN:
            return True
        return self.headers.get("Authorization") == f"Bearer {PLUGIN_TOKEN}"

    def _json(self, status, body):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _body(self):
        length = int(self.headers.get("Content-Length", "0"))
        return json.loads(self.rfile.read(length) or b"{}")

    def do_GET(self):
        if not self._auth():
            return self._json(401, {"error": "unauthorized"})
        if self.path == "/v1/plugin/manifest":
            return self._json(200, MANIFEST)
        if self.path == "/v1/plugin/health":
            return self._json(200, {"status": "ok", "plugin_id": PLUGIN_ID, "details": {"upstream_configured": bool(UPSTREAM_URL), "simulated": SIMULATED_AI and not bool(UPSTREAM_URL)}})
        self._json(404, {"error": "not found"})

    def do_POST(self):
        if not self._auth():
            return self._json(401, {"error": "unauthorized"})
        if self.path != "/v1/ai/analyze":
            return self._json(404, {"error": "not found"})
        body = self._body()
        if not UPSTREAM_URL:
            if SIMULATED_AI:
                # Explicit local-demo mode. These are synthetic detections and MUST NOT be presented as model output.
                return self._json(200, {
                    "plugin_id": PLUGIN_ID,
                    "model": "simulated-demo",
                    "detections": [
                        {"label": "person", "confidence": 0.94, "bbox": {"x": 0.18, "y": 0.16, "width": 0.24, "height": 0.68}, "attributes": {}},
                        {"label": "vehicle", "confidence": 0.87, "bbox": {"x": 0.53, "y": 0.48, "width": 0.37, "height": 0.33}, "attributes": {}},
                    ],
                    "metadata": {"mode": "simulated", "warning": "Demo output; no AI model executed"},
                })
            # Safe no-op mode for validating the plugin path before a real model is connected.
            return self._json(200, {
                "plugin_id": PLUGIN_ID,
                "model": "adapter-noop",
                "detections": [],
                "metadata": {"mode": "noop", "message": "Set UPSTREAM_AI_URL to connect your model"},
            })

        headers = {"Content-Type": "application/json"}
        if UPSTREAM_TOKEN:
            headers["Authorization"] = f"Bearer {UPSTREAM_TOKEN}"
        req = urllib.request.Request(UPSTREAM_URL, data=json.dumps(body).encode(), headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=30) as response:
                upstream = json.loads(response.read())
            # The upstream may already return the VMS contract. If not, wrap it as metadata.
            if isinstance(upstream, dict) and "detections" in upstream:
                upstream.setdefault("plugin_id", PLUGIN_ID)
                upstream.setdefault("model", None)
                upstream.setdefault("metadata", {})
                return self._json(200, upstream)
            return self._json(200, {"plugin_id": PLUGIN_ID, "model": None, "detections": [], "metadata": {"upstream": upstream}})
        except Exception as exc:
            return self._json(502, {"error": str(exc)})

    def log_message(self, fmt, *args):
        print(f"[{PLUGIN_ID}] {fmt % args}")

if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
