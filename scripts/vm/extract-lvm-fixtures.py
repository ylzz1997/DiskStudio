"""Recover only the independently verified ordinary LVM2 fixture files."""
import base64
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import sys

vm = Path(__file__).resolve().parents[2] / ".vm"
log = Path(sys.argv[1]).resolve(strict=True)
match = re.fullmatch(r"(lvm-images|lvm-raid-matrix|lvm-thin-snapshots|lvm-cow-persistent|lvm-cross-deleted|lvm-raid10-layouts)-4kn-\d+\.log", log.name)
if log.parent != vm / "ubuntu" or not match:
    raise ValueError("Expected the isolated Ubuntu LVM2 serial log")
text = log.read_text(encoding="utf-8", errors="replace")
if "DISKSTUDIO_LVM_LINUX_CHECK_PASS" not in text:
    raise ValueError("Missing Linux data verification")
raid = match[1] == "lvm-raid-matrix"
members = []
for index in range(1, 6 if raid else 5 if match[1] == "lvm-raid10-layouts" else 3):
    marker = f"DS_LVM_{index}"
    if f"DISKSTUDIO_LVM_{index}_FIXTURE_PASS" not in text:
        raise ValueError("Missing LVM2 fixture completion marker")
    manifests = re.findall(rf"^{marker}_MANIFEST (\{{.+\}})$", text, re.MULTILINE)
    if len(manifests) != 1:
        raise ValueError("Expected one manifest per physical volume")
    evidence = json.loads(manifests[0])
    blocks = re.findall(rf"^{marker}_BLOCK (\d{{5}}) ([A-Za-z0-9+/=]+)$", text, re.MULTILINE)
    if evidence["member"] != index or not blocks or any(
            int(number) != offset or len(part) > 768 for offset, (number, part) in enumerate(blocks)):
        raise ValueError("Fixture blocks are incomplete or misordered")
    payload = b"".join(base64.b64decode(part, validate=True) for _, part in blocks)
    if len(payload) != evidence["gzipBytes"] or hashlib.sha256(payload).hexdigest() != evidence["gzipSha256"]:
        raise ValueError("Compressed fixture does not match")
    members.append((payload, evidence))
destination = vm / {
    "lvm-images": "lvm-native",
    "lvm-raid-matrix": "lvm-raid-native",
    "lvm-thin-snapshots": "lvm-thin-native",
    "lvm-cow-persistent": "lvm-cow-native",
    "lvm-cross-deleted": "lvm-cross-native",
    "lvm-raid10-layouts": "lvm-raid10-native",
}[match[1]]
destination.mkdir()
for i, (payload, evidence) in enumerate(members, 1):
    size = 256 * 1024**2
    digest = hashlib.sha256()
    with gzip.GzipFile(fileobj=io.BytesIO(payload)) as stream, (destination / f"pv-{i}.raw").open("xb") as target:
        for _ in range(256):
            chunk = stream.read(1024**2)
            if len(chunk) != 1024**2:
                raise ValueError("Physical volume image was truncated")
            digest.update(chunk)
            target.write(chunk)
        if stream.read(1) or target.tell() != size or digest.hexdigest() != evidence["imageSha256"]:
            raise ValueError("Physical volume image does not match")
    with (destination / f"pv-{i}.json").open("x", encoding="utf-8") as target:
        json.dump(evidence, target, indent=2)
        target.write("\n")
    print(evidence["imageSha256"])
