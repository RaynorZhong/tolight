"""Offline verification of the public signing fixtures. No installation or HTTP."""

import argparse
import hashlib
import json
from pathlib import Path
from urllib.parse import urlsplit

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from minimal_client import b64url, canonical_authority, sign_request, string_to_sign, unb64url


def check(path):
    fixture = json.loads(path.read_text(encoding="utf-8"))
    if fixture["schema"] != "tolight-signing-vectors/1" or fixture["scheme"] != "tolight-participant-v1":
        raise ValueError("unexpected signing-vector schema or scheme")
    names = set()
    coverage = set()
    for vector in fixture["vectors"]:
        name = vector["name"]
        if name in names:
            raise ValueError("duplicate vector name: " + name)
        names.add(name)
        seed = unb64url(vector["seed"], 32)
        body = unb64url(vector["body_base64url"])
        url = urlsplit(vector["url"])
        if vector["authority"] != url.netloc:
            raise ValueError(f"{name}: authority does not match the raw URL authority")
        if vector["method"] == "GET" and url.query:
            coverage.add("get-query")
        if vector["method"] == "POST" and body:
            coverage.add("post-body")
        if url.port is not None:
            coverage.add("default-port" if url.port == {"http": 80, "https": 443}[url.scheme] else "other-port")
        if ":" in url.hostname:
            coverage.add("ipv6")
        key = Ed25519PrivateKey.from_private_bytes(seed)
        public = key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        expected = string_to_sign(vector["method"], vector["url"], vector["timestamp"], body)
        headers = sign_request(seed, vector["method"], vector["url"], vector["timestamp"], body)
        checks = {
            "public_key": b64url(public),
            "body_sha256": hashlib.sha256(body).hexdigest(),
            "string_to_sign": expected,
            "signature": headers["Tolight-Signature"],
            "host": canonical_authority(vector["authority"], vector["url"].split(":", 1)[0]),
        }
        for field, actual in checks.items():
            if vector[field] != actual:
                raise ValueError(f"{name}: {field} does not match")
        for index, field in ((1, "method"), (3, "path"), (4, "query"), (5, "timestamp")):
            if expected.split("\n")[index] != str(vector[field]):
                raise ValueError(f"{name}: {field} does not match URL/signature input")
        key.public_key().verify(unb64url(vector["signature"], 64), expected.encode("utf-8"))
        for alternative in vector.get("equivalent_authorities", []):
            if canonical_authority(alternative, vector["url"].split(":", 1)[0]) != vector["host"]:
                raise ValueError(f"{name}: equivalent authority does not match")
        print("PASS " + name)
    if len(names) < 5:
        raise ValueError("at least five signing vectors are required")
    if coverage != {"get-query", "post-body", "default-port", "other-port", "ipv6"}:
        raise ValueError("missing required signing-vector category")
    print(f"Verified {len(names)} public signing vectors")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", type=Path, default=Path(__file__).resolve().parents[2] / "docs/signing-vectors.json")
    args = parser.parse_args()
    try:
        check(args.path)
    except (AssertionError, KeyError, TypeError, ValueError) as error:
        parser.exit(1, "Signing-vector check failed: " + str(error) + "\n")
