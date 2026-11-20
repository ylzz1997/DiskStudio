#!/usr/bin/env python3
"""Generate Linux md reference data on tmpfs-backed loop members."""
import base64
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import struct
import subprocess
import sys
import tempfile

MIB = 1 << 20
CHUNK = 4096
OFFSET = MIB
MEMBER_SIZE = 12 * MIB
FIRST_MINOR = 100  # mdadm 4.3 accepts numbered devices only through md127.
LAYOUTS = {
    0: "left-asymmetric", 1: "right-asymmetric", 2: "left-symmetric",
    3: "right-symmetric", 4: "parity-first", 5: "parity-last",
    8: "ddf-zero-restart", 9: "ddf-N-restart", 10: "ddf-N-continue",
    16: "left-asymmetric-6", 17: "right-asymmetric-6",
    18: "left-symmetric-6", 19: "right-symmetric-6", 20: "parity-first-6",
}


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def run(*args):
    subprocess.run(args, check=True, stdin=subprocess.DEVNULL)


def preflight():
    uuid = Path("/sys/class/dmi/id/product_uuid").read_text().strip().lower()
    disks = json.loads(output("lsblk", "-J", "-b", "-o", "NAME,PATH,TYPE,SERIAL,SIZE"))["blockdevices"]
    disks = [disk for disk in disks if disk["type"] == "disk"]
    if (uuid != "25c39913-ac45-4445-aa54-bdd0dbca7001" or len(disks) != 1
            or disks[0]["serial"] != "DS-UBUNTU-OS-0001" or disks[0]["size"] != 16 << 30):
        raise RuntimeError("Unexpected Ubuntu fixture inventory")
    root = output("findmnt", "-n", "-o", "SOURCE", "/")
    if root not in [item["path"] for item in disks[0].get("children", [])]:
        raise RuntimeError("Unexpected Ubuntu root attachment")
    if output("findmnt", "-n", "-o", "FSTYPE", "--target", "/dev/shm") != "tmpfs":
        raise RuntimeError("The fixture directory must be on tmpfs")
    print(output("mdadm", "--version"), flush=True)


def fixture_members(folder, sizes):
    for slot, size in enumerate(sizes):
        path = folder / f"{slot}.raw"
        with path.open("xb") as stream:
            stream.truncate(size)
        yield path


def mapped_members(sys, paths, devices, length, expected_offset):
    for slot, (path, device) in enumerate(zip(paths, devices)):
        member = sys / "md" / f"dev-{Path(device).name}"
        if (member / "slot").read_text().strip() != str(slot):
            raise RuntimeError("Unexpected md member slot")
        if int((member / "offset").read_text()) * 512 != expected_offset:
            raise RuntimeError("Unexpected md data offset")
        with path.open("rb") as stream:
            stream.seek(expected_offset)
            yield stream.read(length)


def seed_raid10_layout(paths, layout):
    """Set an md v1.2 layout that mdadm's creation UI cannot express."""
    for path in paths:
        with path.open('r+b') as stream:
            stream.seek(4096)
            metadata = bytearray(stream.read(4096))
            magic, version, features = struct.unpack_from('<III', metadata)
            level, code = struct.unpack_from('<II', metadata, 72)
            width = struct.unpack_from('<I', metadata, 92)[0]
            role_slots = struct.unpack_from('<I', metadata, 220)[0]
            if ((magic, version, features, level, code) != (0xA92B4EFC, 1, 0, 10, 0x102)
                    or width != len(paths) or not width <= role_slots <= (4096 - 256) // 2):
                raise RuntimeError('Unexpected baseline RAID10 superblock')
            size = 256 + 2 * role_slots
            def checksum():
                words = (size // 4) * 4
                total = sum(value for (value,) in struct.iter_unpack('<I', metadata[:words]))
                total += int.from_bytes(metadata[words:size], 'little')
                return (total + (total >> 32)) & 0xFFFFFFFF
            stored = struct.unpack_from('<I', metadata, 216)[0]
            struct.pack_into('<I', metadata, 216, 0)
            if checksum() != stored:
                raise RuntimeError('Unexpected baseline checksum')
            struct.pack_into('<I', metadata, 76, layout)
            struct.pack_into('<I', metadata, 216, checksum())
            stream.seek(4096)
            if stream.write(metadata) != len(metadata):
                raise RuntimeError('Incomplete temp member metadata write')
            stream.flush()
            os.fsync(stream.fileno())


def build_array(folder, minor, layout, payload, filesystem=None, *,
                version="1.2", level=6, width=5, layout_name=None, keep_metadata=False,
                sizes=None, stripe=CHUNK, filesystem_offset=0, advanced_raid10=False):
    name = f"md{minor}"
    array_path = Path("/dev") / name
    sys = Path("/sys/block") / name
    if sys.exists() or array_path.exists():
        raise RuntimeError("The fixture array device is already in use")
    sizes = sizes or (16 * MIB,) * width
    if len(sizes) != width or any(not 4 * MIB <= size <= 32 * MIB for size in sizes):
        raise RuntimeError("Unexpected fixture sizes")
    paths = list(fixture_members(folder, sizes))
    devices = []
    started = False
    try:
        for path in paths:
            device = output("losetup", "--find", "--show", "--sector-size", str(CHUNK), str(path))
            if not re.fullmatch(r"/dev/loop\d+", device):
                raise RuntimeError("Expected a loop device")
            loops = json.loads(output("losetup", "--list", "--json", "-O", "NAME,BACK-FILE", device))
            if [(item["name"], item["back-file"]) for item in loops["loopdevices"]] != [(device, str(path))]:
                raise RuntimeError("Unexpected loop backing")
            devices.append(device)
        if sys.exists() or array_path.exists():
            raise RuntimeError("The fixture array device is already in use")
        args = ["mdadm", "--create", str(array_path), "--run", f"--metadata={version}",
                f"--level={level}", f"--raid-devices={width}"]
        if level != 1:
            args += [f"--chunk={stripe // 1024}"]
        if layout_name is not None or level == 6:
            args += [f"--layout={layout_name or LAYOUTS[layout]}"]
        if level >= 1:
            args += ["--assume-clean", "--bitmap=none", "--size=12288"]
        if version != "1.0":
            args += ["--data-offset=1M"]
        run(*args, *devices)
        metadata = array_path.stat()
        if not stat.S_ISBLK(metadata.st_mode):
            raise RuntimeError("Expected a block array")
        # Resolve and verify the kernel device before opening it for writes.
        number = (sys / "dev").read_text().strip()
        device = metadata.st_rdev
        if number != f"{os.major(device)}:{os.minor(device)}":
            raise RuntimeError("Unexpected array path")
        if {entry.name for entry in (sys / "slaves").iterdir()} != {Path(d).name for d in devices}:
            raise RuntimeError("Unexpected md array members")
        started = True
        if advanced_raid10:
            if level != 10 or version != '1.2' or layout_name != 'n2':
                raise RuntimeError('Unexpected fixture baseline')
            run('mdadm', '--stop', str(array_path))
            started = False
            if any(any((Path('/sys/block') / Path(device).name / 'holders').iterdir())
                   for device in devices):
                raise RuntimeError('A temporary RAID10 member still has a holder')
            seed_raid10_layout(paths, layout)
            run('mdadm', '--assemble', '--run', '--config=none', str(array_path), *devices)
            metadata = array_path.stat()
            number = (sys / 'dev').read_text().strip()
            if not stat.S_ISBLK(metadata.st_mode) or number != f'{os.major(metadata.st_rdev)}:{os.minor(metadata.st_rdev)}':
                raise RuntimeError('Unexpected assembled RAID10 path')
            if {entry.name for entry in (sys / 'slaves').iterdir()} != {Path(d).name for d in devices}:
                raise RuntimeError('Unexpected assembled RAID10 members')
            started = True
        expected_metadata = {"level": f"raid{level}", "raid_disks": str(width),
                             "chunk_size": str(0 if level == 1 else stripe),
                             "metadata_version": version}
        if layout is not None:
            expected_metadata["layout"] = str(layout)
        if level >= 1:
            expected_metadata["sync_action"] = "idle"
        for key, expected in expected_metadata.items():
            if (sys / "md" / key).read_text().strip() != expected:
                raise RuntimeError(f"Unexpected md {key}")
        array_size = int((sys / "size").read_text()) * 512
        if not len(payload) <= array_size <= sum(sizes):
            raise RuntimeError("Unexpected md array size")
        if not keep_metadata and array_size != 3 * MEMBER_SIZE:
            raise RuntimeError("Unexpected RAID6 array size")
        with array_path.open("r+b") as array:
            if array.write(payload) != len(payload):
                raise RuntimeError("Incomplete array write")
            array.flush()
            os.fsync(array.fileno())
            array.seek(0)
            readback = array.read(len(payload))
            if readback != payload:
                raise RuntimeError("Logical Linux md readback differs")
        if filesystem is not None:
            fs_path = array_path
            if filesystem_offset:
                fs_path = folder / "readback.ext4"
                with fs_path.open("xb") as stream:
                    stream.write(readback[filesystem_offset:filesystem_offset + 16 * MIB])
            run("e2fsck", "-f", "-n", str(fs_path))
            run("debugfs", "-R", f"dump /nested/proof.bin {folder / 'proof.out'}", str(fs_path))
            if (folder / "proof.out").read_bytes() != filesystem:
                raise RuntimeError("Filesystem readback differs")
        expected_offset = 0 if version == "1.0" else OFFSET
        if keep_metadata:
            list(mapped_members(sys, paths, devices, 0, expected_offset))
            print(f"DS_MD_ARRAY_{minor}_SIZE {array_size}", flush=True)
            if len(readback) == array_size:
                print(f"DS_MD_ARRAY_{minor}_SHA256 {hashlib.sha256(readback).hexdigest()}", flush=True)
            return paths
        length = MEMBER_SIZE if filesystem is not None else len(payload) // 3
        return list(mapped_members(sys, paths, devices, length, expected_offset))
    finally:
        if started:
            run("mdadm", "--stop", str(array_path))
        for device in reversed(devices):
            run("losetup", "--detach", device)


def archive(data, marker):
    compressed = gzip.compress(data, compresslevel=5, mtime=0)
    print(marker + "_MANIFEST " + json.dumps({
        "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
        "gzipSha256": hashlib.sha256(compressed).hexdigest(),
    }))
    for index, start in enumerate(range(0, len(compressed), 576)):
        block = base64.b64encode(compressed[start:start + 576]).decode("ascii")
        print(f"{marker}_BLOCK {index:05d} {block}")
    print(marker + "_PASS", flush=True)


def main():
    preflight()
    if sys.argv[1:] == ["virtual-raid-md-zones-4kn"]:
        zoned_fixtures()
        return
    if sys.argv[1:] == ["virtual-raid-md10-4kn"]:
        raid10_fixtures()
        return
    if sys.argv[1:] == ["virtual-raid-md10-advanced-4kn"]:
        advanced_raid10_fixtures()
        return
    # Every logical chunk has four distinct, reproducible sector fragments.
    pattern = b"".join(hashlib.sha256(str(i).encode()).digest() * 32 for i in range(16 * 3 * 4))
    with tempfile.TemporaryDirectory(dir="/dev/shm", prefix="ds-md6-") as name:
        folder = Path(name)
        tree = folder / "tree" / "nested"
        tree.mkdir(parents=True)
        proof = pattern * 6
        (tree / "proof.bin").write_bytes(proof)
        fs = folder / "source.raw"
        with fs.open("xb") as stream:
            stream.truncate(32 * MIB)
        run("mkfs.ext4", "-q", "-b", "4096", "-d", str(tree.parent), str(fs))
        if sys.argv[1:] == ["virtual-raid-md-metadata-4kn"]:
            metadata_fixtures(folder, pattern, fs.read_bytes(), proof)
            return
        for index, layout in enumerate(LAYOUTS):
            with tempfile.TemporaryDirectory(dir=folder) as work:
                members = build_array(Path(work), FIRST_MINOR + index, layout, pattern)
                archive(b"".join(members), f"DS_MD6_VECTOR_{layout}")
        with tempfile.TemporaryDirectory(dir=folder) as work:
            members = build_array(Path(work), FIRST_MINOR + len(LAYOUTS), 2, fs.read_bytes(), proof)
            for slot, data in enumerate(members):
                archive(data, f"DS_MD6_EXT4_{slot}")
        print("DS_MD6_PROOF " + hashlib.sha256(proof).hexdigest(), flush=True)
    print("DISKSTUDIO_MD6_FIXTURE_PASS", flush=True)


def metadata_fixtures(folder, pattern, filesystem, proof):
    cases = [
        ("native", "1.2", 6, 5, 2, None),
        ("raid5", "1.0", 5, 3, 4, "parity-first"),
        ("raid1", "1.1", 1, 2, None, None),
        ("raid0", "1.2", 0, 3, None, None),
        ("raid4", "1.2", 4, 3, None, None),
        ("raid10", "1.2", 10, 4, 258, "n2"),
    ]
    for index, (label, version, level, width, layout, name) in enumerate(cases):
        native = label == "native"
        minor = FIRST_MINOR + index
        with tempfile.TemporaryDirectory(dir=folder) as work:
            members = build_array(Path(work), minor, layout, filesystem if native else pattern,
                                  proof if native else None, version=version, level=level,
                                  width=width, layout_name=name, keep_metadata=True)
            for slot, path in enumerate(members):
                archive(path.read_bytes(), f"DS_MD_MEMBER_{label}_{slot}")
    print("DS_MD6_PROOF " + hashlib.sha256(proof).hexdigest(), flush=True)
    print("DISKSTUDIO_MD_METADATA_FIXTURE_PASS", flush=True)


def zoned_fixtures():
    sizes = (20 * MIB, 6 * MIB, 13 * MIB)
    offset = 17 * MIB
    capacity = sum(sizes) - len(sizes) * OFFSET
    proof = b"".join(hashlib.sha256(f"restored-{i}".encode()).digest() * 2048 for i in range(18))
    with tempfile.TemporaryDirectory(dir="/dev/shm", prefix="ds-md-zones-") as name:
        folder = Path(name)
        tree = folder / "tree" / "nested"
        tree.mkdir(parents=True)
        (tree / "proof.bin").write_bytes(proof)
        fs = folder / "source.ext4"
        with fs.open("xb") as stream:
            stream.truncate(16 * MIB)
        run("mkfs.ext4", "-q", "-b", "4096", "-d", str(tree.parent), str(fs))
        # Each MiB has a unique marker, including the stripes at both zone boundaries.
        payload = bytearray().join(
            hashlib.sha256(f"chunk-{i}".encode()).digest() * (MIB // 32)
            for i in range(capacity // MIB)
        )
        payload[offset:offset + 16 * MIB] = fs.read_bytes()
        for index, layout in enumerate(("original", "alternate"), start=1):
            with tempfile.TemporaryDirectory(dir=folder) as work:
                members = build_array(Path(work), FIRST_MINOR + index, index, payload, proof,
                                      level=0, width=len(sizes), layout_name=layout,
                                      keep_metadata=True, sizes=sizes, stripe=MIB,
                                      filesystem_offset=offset)
                for slot, path in enumerate(members):
                    archive(path.read_bytes(), f"DS_MD_ZONES_{layout}_{slot}")
        print("DS_MD6_PROOF " + hashlib.sha256(proof).hexdigest(), flush=True)
    print("DISKSTUDIO_MD_ZONES_FIXTURE_PASS", flush=True)


def raid10_fixtures():
    width = 5
    stripe = 64 * 1024
    cases = [("n2", 0x102, 30 * MIB), ("n3", 0x103, 20 * MIB),
             ("f3", 0x301, 20 * MIB), ("o2", 0x10201, 30 * MIB)]
    proof = b"".join(hashlib.sha256(f"proof-{i}".encode()).digest() * 2048 for i in range(18))
    with tempfile.TemporaryDirectory(dir="/dev/shm", prefix="ds-md10-") as name:
        folder = Path(name)
        tree = folder / "tree" / "nested"
        tree.mkdir(parents=True)
        (tree / "proof.bin").write_bytes(proof)
        fs = folder / "source.ext4"
        with fs.open("xb") as stream:
            stream.truncate(16 * MIB)
        run("mkfs.ext4", "-q", "-b", "4096", "-d", str(tree.parent), str(fs))
        for index, (label, layout, capacity) in enumerate(cases):
            native = label == "o2"
            payload = bytearray().join(
                hashlib.sha256(f"{label}-{chunk}".encode()).digest() * (stripe // 32)
                for chunk in range(capacity // stripe)
            )
            if native:
                payload[:16 * MIB] = fs.read_bytes()
            with tempfile.TemporaryDirectory(dir=folder) as work:
                members = build_array(Path(work), FIRST_MINOR + index, layout, payload,
                                      proof if native else None, level=10, width=width,
                                      layout_name=label, keep_metadata=True, stripe=stripe)
                for slot, path in enumerate(members):
                    archive(path.read_bytes(), f"DS_MD_RAID10_{label}_{slot}")
        print("DS_MD6_PROOF " + hashlib.sha256(proof).hexdigest(), flush=True)
    print("DISKSTUDIO_MD_RAID10_FIXTURE_PASS", flush=True)


def advanced_raid10_fixtures():
    stripe = 64 * 1024
    cases = [('mixed-far', 0x202, 6, 18), ('mixed-offset', 0x10202, 6, 18),
             ('legacy-far', 0x20201, 6, 36), ('fixed-mixed', 0x40202, 8, 24)]
    proof = b''.join(hashlib.sha256(f'advanced-{i}'.encode()).digest() * 2048 for i in range(18))
    with tempfile.TemporaryDirectory(dir='/dev/shm', prefix='ds-md10-advanced-') as name:
        folder = Path(name)
        tree = folder / 'tree' / 'nested'
        tree.mkdir(parents=True)
        (tree / 'proof.bin').write_bytes(proof)
        image = folder / 'source.ext4'
        with image.open('xb') as stream:
            stream.truncate(16 * MIB)
        run('mkfs.ext4', '-q', '-b', '4096', '-d', str(tree.parent), str(image))
        for index, (label, layout, width, capacity_mib) in enumerate(cases):
            native = label == 'mixed-offset'
            payload = bytearray().join(
                hashlib.sha256(f'{label}-{chunk}'.encode()).digest() * (stripe // 32)
                for chunk in range(capacity_mib * MIB // stripe)
            )
            if native:
                payload[:16 * MIB] = image.read_bytes()
            with tempfile.TemporaryDirectory(dir=folder) as work:
                members = build_array(Path(work), FIRST_MINOR + index, layout, payload,
                                      proof if native else None, level=10, width=width,
                                      layout_name='n2', keep_metadata=True, stripe=stripe,
                                      advanced_raid10=True)
                for slot, path in enumerate(members):
                    archive(path.read_bytes(), f'DS_MD_ADVANCED_{label}_{slot}')
        print('DS_MD6_PROOF ' + hashlib.sha256(proof).hexdigest(), flush=True)
    print('DISKSTUDIO_MD_RAID10_ADVANCED_FIXTURE_PASS', flush=True)


if __name__ == "__main__":
    main()
