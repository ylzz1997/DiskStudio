#!/usr/bin/env python3
"""Build and independently read LVM2 fixtures on ordinary files in isolated Ubuntu."""
import base64
import gzip
import hashlib
import json
import pathlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time

MIB = 1 << 20
VG = "ds_lvm_fixture"
FILES = {
    "linear": b"DiskStudio LVM2 linear fixture\n",
    "striped": b"".join(bytes([n]) * 65536 for n in range(1, 19)),
}
RAID_CASES = {
    "r0": ("raid0", ("-i", "2")),
    "r0meta": ("raid0_meta", ("-i", "2")),
    "r1": ("raid1", ("-m", "1")),
    "r4": ("raid4", ("-i", "2")),
    "r5": ("raid5_ls", ("-i", "2")),
    "r6": ("raid6_zr", ("-i", "3")),
    "r10": ("raid10", ("-i", "2", "-m", "1")),
}
RAID = sys.argv[1:] == ["lvm-raid-matrix-4kn"]
THIN = sys.argv[1:] == ["lvm-thin-snapshots-4kn"]
CLASSIC = sys.argv[1:] == ["lvm-cow-persistent-4kn"]
DELETED = sys.argv[1:] == ["lvm-cross-deleted-4kn"]
RAID10 = sys.argv[1:] == ["lvm-raid10-layouts-4kn"]
if RAID:
    VG = "ds_lvm_raid_fixture"
    FILES = {name: b"".join(bytes([n + index]) * 65536 for n in range(1, 19))
             for index, name in enumerate(RAID_CASES)}
elif THIN:
    VG = "ds_lvm_thin_fixture"
    FILES = {
        "point1": b"".join(bytes([n]) * 65536 for n in range(1, 19)),
        "origin": b"".join(bytes([n + 32]) * 65536 for n in range(1, 19)),
    }
elif CLASSIC:
    VG = "ds_lvm_cow_fixture"
    FILES = {name: b"".join(bytes([n + index * 32]) * 65536 for n in range(1, 19))
             for index, name in enumerate(("point1", "point2", "origin", "sparse"))}
elif DELETED:
    VG = "ds_lvm_deleted_fixture"
    FILES = {"deleted": b"".join(bytes([n]) * 65536 for n in range(1, 19))}
elif RAID10:
    VG = "ds_lvm_raid10_fixture"
    FILES = {name: b"".join(bytes([n + index * 32]) * 65536 for n in range(1, 19))
             for index, name in enumerate(("far", "offset"))}


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def run(*args):
    subprocess.run(args, check=True)


def preflight():
    uuid = pathlib.Path("/sys/class/dmi/id/product_uuid").read_text().strip().lower()
    if uuid != "25c39913-ac45-4445-aa54-bdd0dbca7001":
        raise RuntimeError("Unexpected Ubuntu VM UUID")
    tree = json.loads(output("lsblk", "-J", "-b", "-o", "NAME,PATH,TYPE,SERIAL,SIZE"))["blockdevices"]
    disks = [item for item in tree if item["type"] == "disk"]
    if len(disks) != 1 or disks[0]["serial"] != "DS-UBUNTU-OS-0001" or disks[0]["size"] != 16 << 30:
        raise RuntimeError("Only the isolated Ubuntu OS may be present")
    root = output("findmnt", "-n", "-o", "SOURCE", "/")
    if root not in [item["path"] for item in disks[0].get("children", [])]:
        raise RuntimeError("Unexpected Ubuntu root attachment")
    if output("findmnt", "-n", "-o", "FSTYPE", "--target", "/dev/shm") != "tmpfs":
        raise RuntimeError("The fixtures must use ordinary tmpfs files")


def archive(image, index):
    compressed = image.with_suffix(".gz")
    with image.open("rb") as stream, gzip.open(compressed, "wb", compresslevel=5) as target:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
        stream.seek(0)
        shutil.copyfileobj(stream, target, MIB)
    payload = compressed.read_bytes()
    marker = f"DS_LVM_{index}"
    print(marker + "_MANIFEST " + json.dumps({
        "member": index,
        "imageSha256": digest,
        "gzipSha256": hashlib.sha256(payload).hexdigest(),
        "gzipBytes": len(payload),
        "files": {name: {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                  for name, data in FILES.items()},
    }), flush=True)
    for n, start in enumerate(range(0, len(payload), 576)):
        print(f"{marker}_BLOCK {n:05d} {base64.b64encode(payload[start:start+576]).decode('ascii')}")
    print(f"DISKSTUDIO_LVM_{index}_FIXTURE_PASS", flush=True)


def readback(path, folder, name, data):
    run("e2fsck", "-f", "-n", path)
    copied = folder / f"{name}.check"
    run("debugfs", "-R", f"dump /nested/{name}.bin {copied}", path)
    if copied.read_bytes() != data:
        raise RuntimeError("Linux logical volume readback differs")


def build_thin(folder, devices, options):
    run("lvcreate", *options, "-L", "8M", "-n", "poolmeta", VG, devices[0])
    run("lvcreate", *options, "-L", "160M", "-n", "pool", VG, devices[1])
    run("lvconvert", *options, "--yes", "--type", "thin-pool", "--poolmetadata",
        f"{VG}/poolmeta", "--chunksize", "64K", f"{VG}/pool")
    run("lvcreate", *options, "--yes", "-V", "96M", "--thinpool", f"{VG}/pool", "-n", "origin")
    origin = f"/dev/{VG}/origin"
    snapshot = f"/dev/{VG}/point1"
    source = folder / "tree" / "nested"
    source.mkdir(parents=True)
    (source / "point1.bin").write_bytes(FILES["point1"])
    # A thin snapshot captures the original directory and data blocks.
    run("mkfs.ext4", "-q", "-b", "4096", "-d", str(source.parent), origin)
    run("lvcreate", *options, "--yes", "-s", "-n", "point1", f"{VG}/origin")
    run("lvchange", *options, "-ay", "-K", f"{VG}/point1")
    changed = folder / "origin.bin"
    changed.write_bytes(FILES["origin"])
    run("debugfs", "-w", "-R", "rm /nested/point1.bin", origin)
    run("debugfs", "-w", "-R", f"write {changed} /nested/origin.bin", origin)
    run("sync")
    for name, path in (("origin", origin), ("point1", snapshot)):
        readback(path, folder, name, FILES[name])
        with open(path, "rb", buffering=0) as stream:
            stream.seek(60 * MIB + 65536 - 16)
            if stream.read(32) != bytes(32):
                raise RuntimeError("Unprovisioned thin range was not zero")
    # Record the authoritative LVM configuration next to the digest evidence.
    run("vgcfgbackup", *options, "-f", str(folder / "thin.lvm"), VG)
    print((folder / "thin.lvm").read_text(), flush=True)


def build_classic(folder, devices, options):
    run("lvcreate", *options, "-L", "96M", "-n", "origin", VG, devices[0])
    origin = f"/dev/{VG}/origin"
    source = folder / "tree" / "nested"
    source.mkdir(parents=True)
    (source / "point1.bin").write_bytes(FILES["point1"])
    # Keep a region beyond Ext4 for a deterministic multi-area COW exercise.
    run("mkfs.ext4", "-q", "-b", "4096", "-d", str(source.parent), origin, "20480")
    run("sync")
    histories = (("point1", "4K", 0x31), ("point2", "64K", 0x72), ("origin", None, 0xb4))
    previous = "point1"
    for name, chunk, tail_byte in histories:
        if name != previous:
            changed = folder / f"{name}.bin"
            changed.write_bytes(FILES[name])
            run("debugfs", "-w", "-R", f"rm /nested/{previous}.bin", origin)
            run("debugfs", "-w", "-R", f"write {changed} /nested/{name}.bin", origin)
        with open(origin, "r+b", buffering=0) as stream:
            stream.seek(84 * MIB)
            if stream.write(bytes([tail_byte]) * (2 * MIB)) != 2 * MIB:
                raise RuntimeError("Short fixture-only tail write")
        run("sync")
        if chunk:
            run("lvcreate", *options, "--yes", "-s", "-L", "64M", "-c", chunk,
                "-n", name, f"{VG}/origin", devices[1])
        previous = name
    for name, _, tail_byte in histories:
        path = f"/dev/{VG}/{name}"
        readback(path, folder, name, FILES[name])
        with open(path, "rb", buffering=0) as stream:
            stream.seek(84 * MIB)
            if stream.read(2 * MIB) != bytes([tail_byte]) * (2 * MIB):
                raise RuntimeError("Linux snapshot tail readback differs")
    run("lvcreate", *options, "--yes", "--type", "snapshot", "-V", "64M", "-L", "32M", "-c",
        "16K", "-n", "sparse", VG, devices[1])
    sparse = f"/dev/{VG}/sparse"
    source = folder / "sparse-tree" / "nested"
    source.mkdir(parents=True)
    (source / "sparse.bin").write_bytes(FILES["sparse"])
    run("mkfs.ext4", "-q", "-b", "4096", "-d", str(source.parent), sparse, "12288")
    readback(sparse, folder, "sparse", FILES["sparse"])
    with open(sparse, "rb", buffering=0) as stream:
        stream.seek(56 * MIB + 16384 - 16)
        if stream.read(32) != bytes(32):
            raise RuntimeError("Linux sparse snapshot did not read unwritten space as zero")
    run("vgcfgbackup", *options, "-f", str(folder / "classic.lvm"), VG)
    print((folder / "classic.lvm").read_text(), flush=True)


def build_deleted(folder, devices, options):
    # The two PVs carry alternate 64 KiB stripes of one logical volume.
    run("lvcreate", *options, "-i", "2", "-I", "64", "-L", "384M", "-n", "history", VG, *devices)
    path = f"/dev/{VG}/history"
    layout = output("lvs", *options, "--noheadings", "--separator", ":", "-o", "segtype,stripes,devices", path)
    kind, stripes, members = (item.strip() for item in layout.split(":"))
    if (kind != "striped" or stripes != "2"
            or sorted(item.split("(")[0] for item in members.split(",")) != sorted(devices)
            or int(output("blockdev", "--getsize64", path)) != 384 * MIB):
        raise RuntimeError(f"Unexpected striped volume: {layout}")
    run("mkfs.btrfs", "--nodiscard", "--checksum", "sha256", "-d", "single", "-m", "dup",
        "-s", "4096", "-n", "16384", "-L", "DSLVMHISTORY", path)
    mount = folder / "history"
    mount.mkdir()
    run("mount", "-t", "btrfs", "-o", "noatime,nodiscard", path, str(mount))
    try:
        live, frozen = mount / "live", mount / "frozen"
        run("btrfs", "subvolume", "create", str(live))
        nested = live / "nested"
        nested.mkdir()
        deleted = nested / "deleted.bin"
        deleted.write_bytes(FILES["deleted"])
        (nested / "rename-old.txt").write_text("Renamed live entry\n")
        (nested / "live.txt").write_text("Still present in the live tree\n")
        run("btrfs", "subvolume", "snapshot", "-r", str(live), str(frozen))
        deleted.unlink()
        (nested / "rename-old.txt").rename(nested / "rename-new.txt")
    finally:
        run("umount", str(mount))
    run("btrfs", "check", "--readonly", "--check-data-csum", path)
    run("mount", "-t", "btrfs", "-o", "ro,nologreplay,noatime", path, str(mount))
    try:
        if (output("btrfs", "property", "get", "-ts", str(frozen), "ro") != "ro=true"
                or (frozen / "nested" / "deleted.bin").read_bytes() != FILES["deleted"]
                or deleted.exists()
                or sorted(item.name for item in nested.iterdir()) != ["live.txt", "rename-new.txt"]):
            raise RuntimeError("Read-only snapshot or live controls differ")
    finally:
        run("umount", str(mount))


def lvm_crc(data):
    checksum = 0xf597a6cf
    for value in data:
        checksum ^= value
        for _ in range(8):
            checksum = (checksum >> 1) ^ (0xedb88320 if checksum & 1 else 0)
    return checksum


def raid10_descriptors(images):
    """Change only two text segment types in the inactive test PVs, preserving the MDA layout."""
    for image in images:
        with image.open("r+b") as stream:
            stream.seek(512)
            label = stream.read(512)
            if label[:8] != b"LABELONE" or label[24:32] != b"LVM2 001":
                raise RuntimeError("Unexpected fixture PV label")
            mda, area_size = struct.unpack_from("<QQ", label, struct.unpack_from("<I", label, 20)[0] + 72)
            stream.seek(mda)
            header = bytearray(stream.read(512))
            if (mda, area_size) != struct.unpack_from("<QQ", header, 24) or lvm_crc(header[4:]) != struct.unpack_from("<I", header)[0]:
                raise RuntimeError("Invalid test PV metadata area")
            offset, length, checksum = struct.unpack_from("<QQI", header, 40)
            if not (512 <= offset < offset + length <= area_size) or length > MIB or any(header[64:88]):
                raise RuntimeError("Unexpected fixture metadata ring position")
            stream.seek(mda + offset)
            original = stream.read(length)
            if lvm_crc(original) != checksum or not original.endswith(b"\0"):
                raise RuntimeError("Fixture metadata checksum differs")
            updated = original
            for name in FILES:
                pattern = rb"(\n\s*" + name.encode() + rb"\s*\{[^}]*?type = )\"raid10(?:_near)?\""
                updated, count = re.subn(pattern, rb'\g<1>"raid10_' + name.encode() + b'"', updated)
                if count != 1:
                    raise RuntimeError(f"Missing exact LVM2 descriptor for {name}")
            if offset + len(updated) > area_size:
                raise RuntimeError("Updated descriptor exceeds metadata area")
            struct.pack_into("<QI", header, 48, len(updated), lvm_crc(updated))
            struct.pack_into("<I", header, 0, lvm_crc(header[4:]))
            stream.seek(mda + offset)
            stream.write(updated)
            stream.seek(mda)
            stream.write(header)


def build_raid10(folder, devices, options):
    # LVM2 allocates the test components. The kernel dm-raid target then writes
    # the far/offset layouts, which the LVM2 CLI cannot create directly.
    for name in FILES:
        run("lvcreate", *options, "--type", "raid10", "-i", "2", "-m", "1", "-I", "64", "-L", "48M", "-n", name, VG, *devices)
    report = json.loads(output("lvs", *options, "--all", "--segments", "--reportformat", "json", "--units", "b", "--nosuffix",
                               "-o", "lv_name,lv_size,segtype,devices,regionsize", VG))["report"][0]["seg"]
    bases = {device: int(output("pvs", *options, "--noheadings", "--units", "b", "--nosuffix", "-o", "pe_start", device)) // 512
             for device in devices}
    tables = {}
    for name in FILES:
        rows = []
        for role in range(4):
            for kind in ("rmeta", "rimage"):
                component, = [row for row in report if row["lv_name"].strip("[]") == f"{name}_{kind}_{role}"]
                device, pe = re.fullmatch(r"(/dev/loop\d+)\((\d+)\)", component["devices"]).groups()
                size = int(component["lv_size"]) // 512
                if component["segtype"] != "linear" or size != (4 if kind == "rmeta" else 24) * MIB // 512:
                    raise RuntimeError("Unexpected LVM2 component layout")
                rows.append((f"ds_{name}_{kind}_{role}", device, bases[device] + int(pe) * (4 * MIB // 512), size, kind))
        volume, = [row for row in report if row["lv_name"] == name]
        region = int(volume["regionsize"]) // 512
        if region < 128 or region & (region - 1):
            raise RuntimeError("Invalid LVM2 bitmap region")
        tables[name] = rows, region
    run("vgchange", *options, "-an", VG)
    for name, data in FILES.items():
        active = []
        try:
            rows, region = tables[name]
            for component, device, sector, size, kind in rows:
                run("dmsetup", "create", component, "--table", f"0 {size} linear {device} {sector}")
                active.append(component)
                if kind == "rmeta":
                    with open(f"/dev/mapper/{component}", "r+b", buffering=0) as stream:
                        stream.write(bytes(4 * MIB))
            # Use the full-array rotation for far, corrected per-set rotation for offset.
            rotation = " raid10_use_near_sets" if name == "far" else ""
            members = " ".join(f"/dev/mapper/{component}" for component in active)
            size = 48 * MIB // 512
            array = f"ds_layout_{name}"
            run("dmsetup", "create", array, "--table",
                f"0 {size} raid raid10 {5 + bool(rotation)} 128 raid10_format {name} region_size {region}{rotation} 4 {members}")
            active.append(array)
            path = f"/dev/mapper/{array}"
            for _ in range(150):
                status = output("dmsetup", "status", array).split()
                if len(status) >= 8 and status[5] == "AAAA" and status[7] == "idle" and len(set(status[6].split("/"))) == 1:
                    break
                time.sleep(1)
            else:
                raise RuntimeError(f"Array did not synchronize: {status}")
            source = folder / name / "nested"
            source.mkdir(parents=True)
            (source / f"{name}.bin").write_bytes(data)
            run("mkfs.ext4", "-q", "-b", "4096", "-d", str(source.parent), path, "8192")
            # A deterministic range at the very end exercises every layout's final row.
            with open(path, "r+b", buffering=0) as stream:
                stream.seek(48 * MIB - len(data))
                if stream.write(data) != len(data):
                    raise RuntimeError("Short write at the end of the fixture LV")
                stream.seek(48 * MIB - len(data))
                if stream.read(len(data)) != data:
                    raise RuntimeError("Linux RAID10 tail differs")
            readback(path, folder, name, data)
            run("sync")
            time.sleep(6)
            print(name, output("dmsetup", "table", array), flush=True)
            print(name, output("dmsetup", "status", array), flush=True)
        finally:
            for device in reversed(active):
                run("dmsetup", "remove", device)


def build(folder):
    images = [folder / f"pv-{i}.raw" for i in range(1, 6 if RAID else 5 if RAID10 else 3)]
    devices = []
    activated = False
    try:
        for image in images:
            with image.open("xb") as stream:
                stream.truncate(256 * MIB)
            device = output("losetup", "--find", "--show", "--sector-size", "4096", str(image))
            if not re.fullmatch(r"/dev/loop\d+", device):
                raise RuntimeError("Unexpected loop device")
            devices.append(device)
            status = json.loads(output("losetup", "--list", "--json", "-O", "NAME,BACK-FILE", device))
            if [(item["name"], item["back-file"]) for item in status["loopdevices"]] != [(device, str(image))]:
                raise RuntimeError("Unexpected loop-device backing")
        options = ("--devices", ",".join(devices), "--config", "devices { use_devicesfile = 0 }")
        run("pvcreate", *options, *devices)
        run("vgcreate", *options, "-s", "4M", VG, *devices)
        activated = True
        if RAID10:
            build_raid10(folder, devices, options)
            activated = False
            raid10_descriptors(images)
        elif DELETED:
            build_deleted(folder, devices, options)
        elif CLASSIC:
            build_classic(folder, devices, options)
        elif THIN:
            build_thin(folder, devices, options)
        elif RAID:
            for name, (kind, arguments) in RAID_CASES.items():
                stripe = () if kind == "raid1" else ("-I", "64")
                run("lvcreate", *options, "--type", kind, *arguments, *stripe,
                    "-L", "48M", "-n", name, VG, *devices)
        else:
            run("lvcreate", *options, "-L", "32M", "-n", "linear", VG, devices[0])
            run("lvcreate", *options, "-i", "2", "-I", "64", "-L", "96M", "-n", "striped", VG, *devices)
        report = [] if RAID10 else json.loads(output(
            "lvs", *options, "--reportformat", "json", "--segments", "--units", "b", "--nosuffix",
            "-o", "vg_name,lv_name,lv_path,lv_size,segtype,devices", VG,
        ))["report"][0]["seg"]
        if not RAID10 and len(report) != len(FILES) + int(THIN):
            raise RuntimeError("Unexpected number of visible logical segments")
        for name, bytes_ in (() if DELETED or RAID10 else FILES.items()):
            segment, = [row for row in report if row["lv_name"] == name]
            path = f"/dev/{VG}/{name}"
            if CLASSIC:
                size = int(output("blockdev", "--getsize64", path))
                if segment["vg_name"] != VG or size != (64 if name == "sparse" else 96) * MIB:
                    raise RuntimeError(f"Unexpected classic snapshot view: {name}, {size}, {segment}")
                continue
            wanted = devices if name == "striped" else devices[:1]
            seen = [entry.partition("(")[0].strip() for entry in segment["devices"].split(",")]
            layout = "thin" if THIN else RAID_CASES[name][0] if RAID else name
            if (segment["vg_name"] != VG or segment["lv_path"] != path
                    or int(segment["lv_size"]) != (96 if THIN else 48 if RAID else 96 if name == "striped" else 32) * MIB
                    or segment["segtype"] != layout
                    or (not THIN and not RAID and sorted(seen) != sorted(wanted))):
                raise RuntimeError("Unexpected logical volume mapping")
            if THIN:
                continue
            source = folder / name / "nested"
            source.mkdir(parents=True)
            (source / f"{name}.bin").write_bytes(bytes_)
            run("mkfs.ext4", "-q", "-b", "4096", "-d", str(source.parent), path)
            if RAID and name not in ("r0", "r0meta"):
                for attempt in range(120):
                    synced = output("lvs", *options, "--noheadings", "-o", "copy_percent", path)
                    if synced == "100.00":
                        break
                    time.sleep(1)
                else:
                    raise RuntimeError(f"RAID did not synchronize: {name}")
            readback(path, folder, name, bytes_)
        print("DISKSTUDIO_LVM_LINUX_CHECK_PASS", flush=True)
    finally:
        if activated:
            run("vgchange", *options, "-an", VG)
        for device in reversed(devices):
            run("losetup", "--detach", device)
    for i, image in enumerate(images, 1):
        archive(image, i)


preflight()
with tempfile.TemporaryDirectory(prefix="diskstudio-lvm-", dir="/dev/shm") as temp:
    build(pathlib.Path(temp))
