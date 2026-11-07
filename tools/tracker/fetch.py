"""Fetches upstream Umami's built tracker and recorder scripts from its release image.

    python3 tools/tracker/fetch.py 3.4.1

Reads the image layer that holds `/app/public` straight from the GitHub container
registry (no Docker or Node needed) and writes `umami-server/assets/script.js`
and `recorder.js`, byte-for-byte as upstream ships them. Update `SCRIPT_SHA256`
and `RECORDER_SHA256` in `umami-server/src/tracker.rs` afterwards.
"""

import hashlib
import io
import json
import pathlib
import sys
import tarfile
import urllib.request

REPO = "umami-software/umami"
REGISTRY = "https://ghcr.io/v2"
ACCEPT = ",".join(
    [
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    ]
)
OUT = pathlib.Path(__file__).resolve().parents[2] / "umami-server" / "assets"
SCRIPTS = ["script.js", "recorder.js"]


def main(tag: str) -> None:
    token = json.load(urllib.request.urlopen(f"https://ghcr.io/token?scope=repository:{REPO}:pull"))["token"]

    def get(path: str, accept: str | None = None) -> bytes:
        headers = {"Authorization": f"Bearer {token}"}
        if accept:
            headers["Accept"] = accept
        return urllib.request.urlopen(urllib.request.Request(f"{REGISTRY}/{REPO}/{path}", headers=headers)).read()

    manifest = json.loads(get(f"manifests/{tag}", ACCEPT))
    if "manifests" in manifest:
        digest = next(m["digest"] for m in manifest["manifests"] if m.get("platform", {}).get("architecture") == "amd64")
        manifest = json.loads(get(f"manifests/{digest}", ACCEPT))

    config = json.loads(get(f"blobs/{manifest['config']['digest']}"))
    history = [h for h in config["history"] if not h.get("empty_layer")]
    index = next(i for i, h in enumerate(history) if "/app/public" in h.get("created_by", ""))
    layer = get(f"blobs/{manifest['layers'][index]['digest']}")

    with tarfile.open(fileobj=io.BytesIO(layer), mode="r:*") as tar:
        for name in SCRIPTS:
            script = tar.extractfile(tar.getmember(f"app/public/{name}")).read()
            out = OUT / name
            out.write_bytes(script)
            print(f"wrote {out} ({len(script)} bytes, sha256 {hashlib.sha256(script).hexdigest()})")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
