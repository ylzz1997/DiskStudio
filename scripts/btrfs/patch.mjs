import fs from 'node:fs';
import path from 'node:path';
const base = process.argv[2];
const directory = path.join(base, 'src/ubtrfs');
let source = fs.readFileSync(path.join(directory, 'ubtrfs.c'), 'utf8').replaceAll('\r\n', '\n');
function replace(original, replacement = '') {
  if (source.split(original).length !== 2) throw Error(original);
  source = source.replace(original, replacement);
}
function between(start, end, value) {
  const a = source.indexOf(start);
  const b = source.indexOf(end, a + start.length);
  if (a < 0 || b < 0) throw Error(start);
  source = source.slice(0, a) + value + source.slice(b);
}
const driverOperations = source.indexOf('static bool look_for_device(');
if (driverOperations < 0) throw Error('WinBtrfs formatter entry point changed');
source = source.slice(0, driverOperations);
replace('#include <ata.h>\n');
replace('#include <mountmgr.h>\n');
replace('#include "../btrfsioctl.h"\n');
replace('#include "../zstd/lib/common/xxhash.h"', '#include <stdio.h>\n#include <errno.h>\n#include <bcrypt.h>');
between('static bool is_ssd(', 'static void add_dir_item(', '');
replace('ssd = is_ssd(h);', 'ssd = false;');
replace('    srand((unsigned int)time(0));\n');
between('static void get_uuid(', 'static void init_device(', `static uint8_t identities[3][16];
static unsigned next_identity;

static void get_uuid(uint8_t* uuid) {
    if (next_identity >= 3) abort();
    memcpy(uuid, identities[next_identity++], 16);
}

`);
between('static void calc_tree_checksum(', 'static NTSTATUS write_roots(', `static void calc_tree_checksum(struct btrfs_header* th, uint32_t node_size) {
    *(uint32_t*)th = ~calc_crc32c(0xffffffff, th->fsid, node_size - sizeof(th->csum));
}

`);
between('static void calc_superblock_checksum(', 'static NTSTATUS write_superblocks(', `static void calc_superblock_checksum(struct btrfs_super_block* sb) {
    *(uint32_t*)sb = ~calc_crc32c(0xffffffff, sb->fsid, sizeof(*sb) - sizeof(sb->csum));
}

`);
replace('if (superblock_addrs[i] > dev->dev_item.total_bytes)', 'if (superblock_addrs[i] > dev->dev_item.total_bytes - sblen)');
replace('    i = 0;\n    while (superblock_addrs[i] != 0) {', `    if (!FlushFileBuffers(h)) { free(sb); return STATUS_IO_DEVICE_ERROR; }
    i = 1;
    for (;;) {
        if (superblock_addrs[i] == 0 || superblock_addrs[i] > dev->dev_item.total_bytes - sblen) {
            // Write the primary only after the new trees and mirrors are durable.
            if (!FlushFileBuffers(h)) { free(sb); return STATUS_IO_DEVICE_ERROR; }
            i = 0;
        }`);
replace('        if (superblock_addrs[i] > dev->dev_item.total_bytes - sblen)\n            break;\n\n');
replace('        i++;\n    }\n\n    free(sb);', '        if (i == 0) break;\n        i++;\n    }\n\n    free(sb);');
between('NTSTATUS NTAPI NtWriteFile(', 'NTSTATUS NTAPI NtReadFile(', `static uint64_t range_start, range_size;
static uint64_t logical_sector;

static NTSTATUS bounded_write(HANDLE handle, HANDLE event, PIO_APC_ROUTINE apc,
                             void* context, PIO_STATUS_BLOCK iosb, void* buffer,
                             ULONG length, PLARGE_INTEGER offset, PULONG key) {
    DWORD written = 0;
    uint64_t start = (uint64_t)offset->QuadPart;
    LARGE_INTEGER at;
    if (start > range_size || length > range_size - start ||
        start % logical_sector || length % logical_sector)
        return STATUS_INVALID_PARAMETER;
    at.QuadPart = (LONGLONG)(range_start + start);
    if (!SetFilePointerEx(handle, at, NULL, FILE_BEGIN) ||
        !WriteFile(handle, buffer, length, &written, NULL) || written != length)
        return STATUS_IO_DEVICE_ERROR;
    return STATUS_SUCCESS;
}
#define NtWriteFile bounded_write

`);
source += `
static int read_decimal(const char* input, uint64_t* result) {
    char* end;
    if (*input < '0' || *input > '9') return 0;
    errno = 0;
    *result = strtoull(input, &end, 10);
    return !errno && !*end;
}

static int hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    return -1;
}

static int decode_hex(const char* input, uint8_t* output, size_t max) {
    size_t length = strlen(input);
    if (length % 2 || length / 2 > max) return -1;
    for (size_t i = 0; i < length / 2; i++) {
        int hi = hex_digit(input[2 * i]), lo = hex_digit(input[2 * i + 1]);
        if (hi < 0 || lo < 0) return -1;
        output[i] = (hi << 4) | lo;
    }
    return (int)(length / 2);
}

int main(int argc, char** argv) {
    WCHAR label_storage[256];
    UNICODE_STRING label = {0};
    HANDLE handle = GetStdHandle(STD_INPUT_HANDLE);
    if (!handle || handle == INVALID_HANDLE_VALUE ||
        GetFileType(handle) != FILE_TYPE_DISK || argc != 6 ||
        !read_decimal(argv[1], &range_start) || !read_decimal(argv[2], &range_size) ||
        !read_decimal(argv[3], &logical_sector) ||
        (logical_sector != 512 && logical_sector != 4096) ||
        range_start % 4096 || range_size % 4096 || range_size < (128ull << 20) ||
        range_size > INT64_MAX || range_start > INT64_MAX - range_size)
        return 2;
    uint8_t utf8_label[255];
    int utf8_length = decode_hex(argv[4], utf8_label, sizeof(utf8_label));
    if (utf8_length < 0) return 2;
    if (utf8_length) {
        for (int i = 0; i < utf8_length; i++)
            if (utf8_label[i] < 32 || utf8_label[i] == 127 ||
                utf8_label[i] == '/' || utf8_label[i] == 92) return 2;
        int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, (char*)utf8_label,
                                         utf8_length, label_storage, 256);
        if (!length) return 2;
        label.Buffer = label_storage;
        label.Length = label.MaximumLength = (USHORT)(length * sizeof(WCHAR));
    }
    if (decode_hex(argv[5], identities[0], 16) != 16 ||
        (identities[0][6] >> 4) != 4 || (identities[0][8] >> 6) != 2)
        return 2;
    if (BCryptGenRandom(NULL, (PUCHAR)&identities[1], sizeof(identities) - 16,
                        BCRYPT_USE_SYSTEM_PREFERRED_RNG) < 0)
        return 3;
    for (size_t i = 1; i < 3; i++) {
        identities[i][6] = (identities[i][6] & 15) | 64;
        identities[i][8] = (identities[i][8] & 63) | 128;
    }
    NTSTATUS status = write_btrfs(handle, range_size, &label, 4096, 16384,
                                 BTRFS_FEATURE_INCOMPAT_EXTENDED_IREF |
                                 BTRFS_FEATURE_INCOMPAT_SKINNY_METADATA |
                                 BTRFS_FEATURE_INCOMPAT_NO_HOLES,
                                 BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE);
    if (!NT_SUCCESS(status)) {
        fprintf(stderr, "Btrfs format failed (0x%08lx)\\n", (unsigned long)status);
        return 4;
    }
    return FlushFileBuffers(handle) ? 0 : 5;
}
`;
fs.writeFileSync(path.join(directory, 'diskstudio-mkbtrfs.c'), source);
