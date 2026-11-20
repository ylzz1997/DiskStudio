"""Generate and verify ordinary WOF backing streams with wimlib 1.14.5."""
import argparse
import ctypes as C
import hashlib
import json
import struct
from pathlib import Path


CHUNKS = (4096, 32768, 8192, 16384)


def bind_codec(path):
    codec = C.CDLL(str(path.resolve(strict=True)))
    buffer = C.c_void_p
    size = C.c_size_t
    handle = C.POINTER(buffer)
    signatures = {
        "create_compressor": ([C.c_int, size, C.c_uint, handle], C.c_int),
        "compress": ([buffer, size, buffer, size, buffer], size),
        "free_compressor": ([buffer], None),
        "create_decompressor": ([C.c_int, size, handle], C.c_int),
        "decompress": ([buffer, size, buffer, size, buffer], C.c_int),
        "free_decompressor": ([buffer], None),
    }
    for name, (args, result) in signatures.items():
        function = getattr(codec, "wimlib_" + name)
        function.argtypes = args
        function.restype = result
    return codec


def payload(algorithm):
    return bytes((i // 113 + i % 41 + algorithm * 17) % 251 for i in range(192 * 1024 + 123))


def encode(codec, data, chunk, encoding):
    compressor = C.c_void_p()
    if codec.wimlib_create_compressor(encoding, chunk, 0, C.byref(compressor)):
        raise ValueError("Could not allocate the wimlib compressor")
    offsets, blocks, length = [], [], 0
    try:
        for offset in range(0, len(data), chunk):
            raw = data[offset:offset + chunk]
            out = C.create_string_buffer(len(raw))
            size = codec.wimlib_compress(raw, len(raw), out, len(raw) - 1, compressor)
            stored = out.raw[:size] if size and len(raw) == chunk else raw
            if offset:
                offsets.append(length)
            blocks.append(stored)
            length += len(stored)
    finally:
        codec.wimlib_free_compressor(compressor)
    if length >= len(data):
        raise ValueError("Expected a compressed payload")
    return struct.pack(f"<{len(offsets)}I", *offsets) + b"".join(blocks)


def decode(codec, stream, size, chunk, encoding):
    count = (size - 1) // chunk
    offsets = [0, *struct.unpack_from(f"<{count}I", stream), len(stream) - count * 4]
    contents = memoryview(stream)[count * 4:]
    decoder = C.c_void_p()
    if codec.wimlib_create_decompressor(encoding, chunk, C.byref(decoder)):
        raise ValueError("Could not allocate the wimlib decompressor")
    try:
        result = bytearray()
        for start, stop in zip(offsets, offsets[1:]):
            raw_size = min(chunk, size - len(result))
            data = bytes(contents[start:stop])
            if len(data) == raw_size:
                result += data
            elif 0 < len(data) < raw_size:
                out = C.create_string_buffer(raw_size)
                if codec.wimlib_decompress(data, len(data), out, raw_size, decoder):
                    raise ValueError("Could not decompress the cloned WOF block")
                result += out.raw
            else:
                raise ValueError("Invalid WOF block length")
        return bytes(result)
    finally:
        codec.wimlib_free_decompressor(decoder)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["generate", "verify"])
    parser.add_argument("dll", type=Path)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    codec = bind_codec(args.dll)
    evidence = []
    for algorithm, chunk in enumerate(CHUNKS):
        raw = payload(algorithm)
        encoding = 2 if algorithm == 1 else 1  # wimlib's LZX and XPRESS constants
        if args.mode == "generate":
            stream = encode(codec, raw, chunk, encoding)
            with (args.directory / f"wof-{algorithm}.bin").open("xb") as out:
                out.write(stream)
            paths = [args.directory / f"wof-{algorithm}.bin"]
        else:
            paths = [args.directory / f"clone-{sector}-{algorithm}.bin" for sector in (512, 4096)]
        for path in paths:
            stream = path.read_bytes()
            restored = decode(codec, stream, len(raw), chunk, encoding)
            if restored != raw:
                raise ValueError(f"Logical contents differ in {path.name}")
            evidence.append({
                "image": path.name,
                "algorithm": algorithm,
                "stored_bytes": len(stream),
                "logical_bytes": len(restored),
                "logical_sha256": hashlib.sha256(restored).hexdigest(),
            })
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
