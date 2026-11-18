#!/usr/bin/env python3
"""Sign a DuckDB extension using its two-level SHA-256 and RSA-2048 footer."""
import hashlib
import pathlib
import subprocess
import sys
import tempfile


def sign(source, destination, private_key):
    data = source.read_bytes()
    if len(data) < 512 or data[-256:] != bytes(256):
        raise ValueError("expected an unsigned DuckDB extension with an empty signature footer")
    payload = data[:-256]
    digest = hashlib.sha256(b"".join(
        hashlib.sha256(payload[i:i + 1024 * 1024]).digest()
        for i in range(0, len(payload), 1024 * 1024)
    )).digest()
    signature = subprocess.run([
        "openssl", "pkeyutl", "-sign", "-inkey", str(private_key),
        "-pkeyopt", "digest:sha256",
    ], input=digest, stdout=subprocess.PIPE, check=True).stdout
    if len(signature) != 256:
        raise ValueError("the signing key must be RSA-2048")
    # Verify before publishing so a mismatched key/tool invocation cannot produce a silent bad artifact.
    with tempfile.TemporaryDirectory(prefix="spatial-sign-") as temporary:
        public = pathlib.Path(temporary) / "public.pem"
        signed = pathlib.Path(temporary) / "signature"
        subprocess.run(["openssl", "pkey", "-in", str(private_key), "-pubout", "-out", str(public)], check=True)
        signed.write_bytes(signature)
        subprocess.run([
            "openssl", "pkeyutl", "-verify", "-pubin", "-inkey", str(public),
            "-sigfile", str(signed), "-pkeyopt", "digest:sha256",
        ], input=digest, check=True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    # A new deployment directory keeps previously approved artifacts intact.
    with destination.open("xb") as output:
        output.write(payload)
        output.write(signature)


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("usage: sign-duckdb-spatial.py UNSIGNED_EXTENSION OUTPUT PRIVATE_KEY")
    sign(*(pathlib.Path(arg) for arg in sys.argv[1:]))
