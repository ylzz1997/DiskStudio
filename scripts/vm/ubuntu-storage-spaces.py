#!/usr/bin/env python3
"""Capture bounded metadata regions from the read-only Windows Storage Spaces fixture."""
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

CAPACITY = 16 << 30
SECTOR = 4096
MIB = 1 << 20
POOL_TYPE = "e75caf8f-f680-4cee-afa3-b001e56efc2d"
OS_SERIAL = "DS-UBUNTU-OS-0001"
MEMBERS = {"DS-TEST-A-0001": "A", "DS-TEST-B-0001": "B"}
SLAB = 256 * MIB
STRIPE = 64 * 1024
INTERNAL_METADATA_PHASE = "storage-spaces-internal-4kn"
MIRROR_CACHE_PHASE = "storage-spaces-cache-4kn"
COMPLETE_MIRROR_CACHE_PHASE = "storage-spaces-cache-complete-4kn"
WITNESS_OFFSETS = (
    0, SECTOR, 60 * 1024, STRIPE, 68 * 1024, 2 * STRIPE, 3 * STRIPE, 4 * STRIPE,
    SLAB - SECTOR, SLAB, SLAB + STRIPE, 2 * SLAB - SECTOR, 2 * SLAB,
    2 * SLAB + STRIPE, 2 * SLAB + 2 * STRIPE, 3 * SLAB, 3 * SLAB + STRIPE, 4 * SLAB - SECTOR,
)


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def inventory():
    uuid = Path("/sys/class/dmi/id/product_uuid").read_text().strip().lower()
    name = Path("/sys/class/dmi/id/product_name").read_text().strip()
    if uuid != "25c39913-ac45-4445-aa54-bdd0dbca7001" or name != "Standard PC (Q35 + ICH9, 2009)":
        raise RuntimeError("Unexpected Ubuntu VM identity")
    tree = json.loads(output("lsblk", "-J", "-b", "-o",
                             "NAME,PATH,TYPE,SERIAL,SIZE,LOG-SEC,PHY-SEC,PARTTYPE,MOUNTPOINTS"))["blockdevices"]
    disks = [row for row in tree if row["type"] == "disk"]
    by_serial = {row["serial"].strip(): row for row in disks if row["serial"]}
    if len(disks) != 3 or set(by_serial) != {OS_SERIAL, *MEMBERS}:
        raise RuntimeError("Expected only the Ubuntu OS and the two disposable NVMe devices")
    os_disk = by_serial[OS_SERIAL]
    root = output("findmnt", "-n", "-o", "SOURCE", "/")
    if os_disk["size"] != CAPACITY or root not in [row["path"] for row in os_disk.get("children", [])]:
        raise RuntimeError("Ubuntu root does not belong to the designated OS disk")
    return by_serial


def region(fd, offset, length):
    if offset % SECTOR or length % SECTOR or not 0 <= offset < offset + length <= CAPACITY:
        raise RuntimeError("Capture range is invalid")
    payload = os.pread(fd, length, offset)
    if len(payload) != length:
        raise RuntimeError("Short read from verified fixture")
    return payload


def witness(kind, index, logical):
    data = bytearray((i * 17 + index * 73 + kind * 31) & 255 for i in range(SECTOR))
    data[:16] = b"DS-SPACES-DATA1\0"
    data[16] = kind
    data[24:32] = logical.to_bytes(8, "little")
    return data


def find_witnesses(fd, start, end):
    candidates = {}
    for kind in (1, 2):
        for index, logical in enumerate(WITNESS_OFFSETS):
            offsets = {logical % SLAB}
            if kind == 2:
                offsets.add((logical % (2 * SLAB) // (2 * STRIPE)) * STRIPE + logical % STRIPE)
            pattern = witness(kind, index, logical)
            for origin in (0, start):
                for slab in range(1, CAPACITY // SLAB):
                    for inside in offsets:
                        offset = origin + slab * SLAB + inside
                        if start + 16 * MIB <= offset <= end - SECTOR:
                            candidates.setdefault(offset, []).append((kind, logical, pattern))
    matches = []
    for offset, options in sorted(candidates.items()):
        data = region(fd, offset, SECTOR)
        if data.startswith(b"DS-SPACES-DATA1\0"):
            for kind, logical, pattern in options:
                if data == pattern:
                    matches.append((kind, logical, offset, data))
                    break
            else:
                raise RuntimeError("Found a fixture marker at an unexpected position")
    if not matches:
        raise RuntimeError("No valid fixture markers found")
    return matches


def scan_witnesses(fd, start, end, originals):
    expected = {(kind, logical): witness(kind, index, logical)
                for kind in (1, 2) for index, logical in enumerate(WITNESS_OFFSETS)}
    results = {offset: (kind, logical, offset, data) for kind, logical, offset, data in originals}
    for base in range(start, end, 16 * MIB):
        data = region(fd, base, min(16 * MIB, end - base))
        pos = data.find(b"DS-SPACES-DATA1\0")
        while pos != -1:
            if pos % SECTOR == 0:
                kind = data[pos + 16]
                logical = int.from_bytes(data[pos + 24:pos + 32], "little")
                pattern = expected.get((kind, logical))
                if pattern is not None and data[pos:pos + SECTOR] == pattern:
                    results[base + pos] = (kind, logical, base + pos, data[pos:pos + SECTOR])
            pos = data.find(b"DS-SPACES-DATA1\0", pos + 1)
        if len(results) > 128:
            raise RuntimeError("Too many marker copies on one test member")
        if (base - start) % (4 << 30) == 0:
            print(f"DS_SPACES_SCAN {base - start} {end - start} {len(results)}", flush=True)
    return [results[offset] for offset in sorted(results)]


def capture(row, phase):
    if row["size"] != CAPACITY or row["log-sec"] != SECTOR or row["phy-sec"] != SECTOR:
        raise RuntimeError("Unexpected member capacity or sector size")
    children = row.get("children", [])
    if any(item["type"] != "part" or any(item["mountpoints"]) or item.get("children")
           for item in children) or any(row["mountpoints"]):
        raise RuntimeError("Fixture partitions must have no mounts or holders")
    pools = [item for item in children if item["parttype"] == POOL_TYPE]
    if len(pools) != 1:
        raise RuntimeError("Expected one Storage Spaces GPT member partition")
    part = pools[0]
    start = int(Path("/sys/class/block", part["name"], "start").read_text()) * 512
    end = start + part["size"]
    if not 64 * 1024 <= start < end <= CAPACITY - 64 * 1024 or part["size"] < 32 * MIB:
        raise RuntimeError("Unexpected pool partition range")
    sysfs = Path("/sys/class/block", row["name"])
    fd = os.open(row["path"], os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        status = os.fstat(fd)
        if not stat.S_ISBLK(status.st_mode) or sysfs.joinpath("dev").read_text().strip() != (
                f"{os.major(status.st_rdev)}:{os.minor(status.st_rdev)}"):
            raise RuntimeError("Verified block device changed before opening")
        if sysfs.joinpath("size").read_text().strip() != str(CAPACITY // 512):
            raise RuntimeError("Verified block device changed capacity")
        gpt = [(0, region(fd, 0, 64 * 1024)),
               (CAPACITY - 64 * 1024, region(fd, CAPACITY - 64 * 1024, 64 * 1024))]
        metadata = []
        for edge in (start, end - 16 * MIB):
            data = region(fd, edge, 16 * MIB)
            for pos in range(0, len(data), SECTOR):
                if data[pos:pos + 7] == b"SPACEDB":
                    offset = edge + pos
                    metadata.append((offset, region(fd, offset, min(MIB, end - offset))))
                    if len(metadata) > 8:
                        raise RuntimeError("Too many metadata candidates")
        if not metadata:
            raise RuntimeError("No sector-aligned SPACEDB metadata within the bounded partition edges")
        if phase == INTERNAL_METADATA_PHASE:
            # The preceding A/B witness scan located later thin-space data in
            # physical block 10. Capture its header and the neighboring block.
            ranges = [(start + (block + 2) * SLAB, 4 * MIB) for block in (10, 11)]
            if any(offset + length > end for offset, length in ranges):
                raise RuntimeError("Internal fixture ranges exceed the verified member partition")
            internal = [(offset, region(fd, offset, length)) for offset, length in ranges]
            return start, end, gpt + metadata, [], internal
        if phase in (MIRROR_CACHE_PHASE, COMPLETE_MIRROR_CACHE_PHASE):
            # Capture the mirror's fixed internal allocations without opening
            # either logical space or changing the Windows pool state.
            lengths = (64 * 1024, 4 * MIB + 2 * SECTOR, 64 * 1024, 64 * 1024, 64 * 1024)
            if phase == MIRROR_CACHE_PHASE:
                lengths = (4 * MIB, 4 * MIB, 64 * 1024, 64 * 1024, 64 * 1024)
            ranges = [(start + (block + 2) * SLAB, length)
                      for block, length in zip((1, 2, 3, 4, 5), lengths)]
            if any(offset + length > end for offset, length in ranges):
                raise RuntimeError("Mirror cache ranges exceed the verified member partition")
            return start, end, gpt + metadata, [], [(offset, region(fd, offset, length)) for offset, length in ranges]
        witnesses = find_witnesses(fd, start, end) if phase != "storage-spaces-metadata-4kn" else []
        if phase == "storage-spaces-witness-full-4kn":
            witnesses = scan_witnesses(fd, start, end, witnesses)
        return start, end, gpt + metadata, witnesses, []
    finally:
        os.close(fd)


def report(slot, capture_result):
    start, end, windows, witnesses, internal = capture_result
    entries = [{"offset": offset, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
               for offset, data in windows]
    archive = gzip.compress(b"".join(data for _, data in windows) +
                            b"".join(data for _, _, _, data in witnesses) +
                            b"".join(data for _, data in internal), mtime=0)
    manifest = {
        "slot": slot, "capacity": CAPACITY, "sector": SECTOR, "partition": [start, end],
        "regions": entries, "gzipBytes": len(archive), "gzipSha256": hashlib.sha256(archive).hexdigest(),
    }
    if witnesses:
        manifest["witnesses"] = [
            {"kind": kind, "logical": logical, "offset": offset, "bytes": len(data),
             "sha256": hashlib.sha256(data).hexdigest()} for kind, logical, offset, data in witnesses
        ]
    if internal:
        manifest["internal"] = [
            {"offset": offset, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
            for offset, data in internal
        ]
    print("DS_SPACES_MANIFEST " + json.dumps(manifest), flush=True)
    for index, offset in enumerate(range(0, len(archive), 576)):
        encoded = base64.b64encode(archive[offset:offset + 576]).decode("ascii")
        print(f"DS_SPACES_BLOCK {slot} {index:05d} {encoded}")


if __name__ == "__main__":
    args = sys.argv[1:]
    if args not in (["storage-spaces-metadata-4kn"], ["storage-spaces-witness-4kn"],
                    ["storage-spaces-witness-full-4kn"], [INTERNAL_METADATA_PHASE],
                    [MIRROR_CACHE_PHASE], [COMPLETE_MIRROR_CACHE_PHASE]):
        raise RuntimeError("Unexpected capture phase")
    disks = inventory()
    observations = [(slot, capture(disks[serial], args[0])) for serial, slot in MEMBERS.items()]
    for slot, observation in observations:
        report(slot, observation)
    print("DISKSTUDIO_STORAGE_SPACES_METADATA_PASS", flush=True)
