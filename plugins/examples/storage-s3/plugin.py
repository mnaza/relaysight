#!/usr/bin/env python3
import json
import os
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import boto3
from botocore.config import Config

PLUGIN_ID = os.getenv("PLUGIN_ID", "storage-s3")
PLUGIN_TOKEN = os.getenv("PLUGIN_TOKEN", "")
PORT = int(os.getenv("PORT", "9002"))
BUCKET = os.environ.get("S3_BUCKET", "vms")
PREFIX = os.getenv("S3_PREFIX", "").strip("/")

INTERNAL_ENDPOINT = os.getenv("S3_ENDPOINT") or None
PUBLIC_ENDPOINT = os.getenv("S3_PUBLIC_ENDPOINT") or INTERNAL_ENDPOINT
EDGE_ENDPOINT = os.getenv("S3_EDGE_ENDPOINT") or PUBLIC_ENDPOINT
SERVICE_ENDPOINT = os.getenv("S3_SERVICE_ENDPOINT") or INTERNAL_ENDPOINT
COMMON = {
    "region_name": os.getenv("AWS_REGION", "us-east-1"),
    "config": Config(signature_version="s3v4", s3={"addressing_style": os.getenv("S3_ADDRESSING_STYLE", "path")}),
}
client = boto3.client("s3", endpoint_url=INTERNAL_ENDPOINT, **COMMON)
signers = {
    "browser": boto3.client("s3", endpoint_url=PUBLIC_ENDPOINT, **COMMON),
    "edge": boto3.client("s3", endpoint_url=EDGE_ENDPOINT, **COMMON),
    "service": boto3.client("s3", endpoint_url=SERVICE_ENDPOINT, **COMMON),
}

MANIFEST = {
    "id": PLUGIN_ID,
    "name": os.getenv("PLUGIN_NAME", "S3 Compatible Storage"),
    "version": "0.2.0",
    "protocol_version": 1,
    "vendor": os.getenv("PLUGIN_VENDOR", "example"),
    "description": "Audience-aware presigned connector for S3, MinIO, Backblaze B2 S3 and compatible storage.",
    "capabilities": ["storage_blob"],
}

def object_key(namespace, key):
    namespace = str(namespace).strip("/")
    key = str(key).lstrip("/")
    parts = [p for p in [PREFIX, namespace, key] if p]
    joined = "/".join(parts)
    if ".." in joined.split("/"):
        raise ValueError("invalid object key")
    return joined

def signer_for(body):
    audience = str(body.get("audience", "service")).lower()
    if audience not in signers:
        raise ValueError(f"unsupported transfer audience: {audience}")
    return signers[audience], audience

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
            try:
                client.head_bucket(Bucket=BUCKET)
                return self._json(200, {
                    "status": "ok", "plugin_id": PLUGIN_ID,
                    "details": {
                        "bucket": BUCKET,
                        "audiences": ["browser", "edge", "service"],
                    },
                })
            except Exception as exc:
                return self._json(503, {"status": "error", "plugin_id": PLUGIN_ID, "details": {"error": str(exc)}})
        return self._json(404, {"error": "not found"})

    def do_POST(self):
        if not self._auth():
            return self._json(401, {"error": "unauthorized"})
        try:
            body = self._body()
            signer, audience = signer_for(body)
            if self.path == "/v1/storage/uploads":
                key = object_key(body.get("namespace", "default"), body["object_key"])
                expires = int(body.get("expires_seconds", 900))
                params = {"Bucket": BUCKET, "Key": key, "ContentType": body.get("content_type", "application/octet-stream")}
                metadata = body.get("metadata") or {}
                upload_headers = {"Content-Type": params["ContentType"]}
                if metadata:
                    params["Metadata"] = {str(k): str(v) for k, v in metadata.items()}
                    upload_headers.update({f"x-amz-meta-{k}": str(v) for k, v in params["Metadata"].items()})
                url = signer.generate_presigned_url("put_object", Params=params, ExpiresIn=expires)
                return self._json(200, transfer("PUT", url, key, expires, upload_headers, audience))
            if self.path == "/v1/storage/downloads":
                key = body["object_ref"]
                expires = int(body.get("expires_seconds", 900))
                url = signer.generate_presigned_url("get_object", Params={"Bucket": BUCKET, "Key": key}, ExpiresIn=expires)
                return self._json(200, transfer("GET", url, key, expires, {}, audience))
            if self.path == "/v1/storage/delete":
                client.delete_object(Bucket=BUCKET, Key=body["object_ref"])
                return self._json(200, {"deleted": True})
            return self._json(404, {"error": "not found"})
        except Exception as exc:
            return self._json(500, {"error": str(exc)})

    def log_message(self, fmt, *args):
        print(f"[{PLUGIN_ID}] {fmt % args}")

def transfer(method, url, key, expires, headers, audience):
    return {
        "method": method,
        "url": url,
        "headers": headers,
        "object_ref": key,
        "expires_at": (datetime.now(timezone.utc) + timedelta(seconds=expires)).isoformat().replace("+00:00", "Z"),
        "audience": audience,
    }

if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
