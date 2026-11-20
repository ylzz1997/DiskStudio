/* SPDX-License-Identifier: GPL-2.0-or-later */
/* A stream-driven NTFS formatter. The parent owns all storage handles. */
#include "config.h"
#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "types.h"
#include "attrib.h"
#include "device.h"
#include "dir.h"
#include "logging.h"
#include "object_id.h"
#include "reparse.h"
#include "efs.h"
#include "security.h"
#include "unistr.h"

#define VOLUME_NAME "diskstudio-volume"
#define MAX_TRANSFER (1024 * 1024)
#define HAS_EXTENDED_ATTRIBUTES 0x00040000u
#define STREAM_LIMIT (UINT64_C(1) << 43)
#define MAX_SPARSE_RANGES 262144u
#define REPARSE_LIMIT (16 * 1024u)

static uint64_t length;
static uint64_t start_lba;
uint64_t diskstudio_volume_serial;
static uint32_t sector_size;
static uint32_t cluster_size;
static int io_failed;
/* Keep the declared sparse attribute even if every logical cluster is allocated. */
const ntfs_attr *diskstudio_sparse_stream;

typedef struct {
    uint64_t position;
} Cursor;

static uint64_t get_le(const unsigned char *bytes, unsigned int count)
{
    uint64_t value = 0;
    for (unsigned int index = 0; index < count; index++)
        value |= (uint64_t)bytes[index] << (8 * index);
    return value;
}

static void put_le(unsigned char *bytes, uint64_t value, unsigned int count)
{
    for (unsigned int index = 0; index < count; index++)
        bytes[index] = (unsigned char)(value >> (8 * index));
}

static int fail(int code)
{
    errno = code;
    return -1;
}

/* Requests are relative to the volume start. A failed exchange is terminal. */
static int exchange(unsigned char operation, uint64_t offset, uint32_t count,
                    void *input, const void *output)
{
    unsigned char request[13] = {operation};
    if (io_failed)
        return fail(EIO);
    put_le(request + 1, offset, 8);
    put_le(request + 9, count, 4);
    if (fwrite(request, 1, sizeof(request), stdout) != sizeof(request)
            || (output && fwrite(output, 1, count, stdout) != count)
            || fflush(stdout) != 0 || fgetc(stdin) != 0
            || (input && fread(input, 1, count, stdin) != count)) {
        io_failed = 1;
        return fail(EIO);
    }
    return 0;
}

static int receive(void *buffer, uint32_t count)
{
    return exchange(4, 0, count, buffer, NULL);
}

static int volume_open(struct ntfs_device *device, int flags)
{
    if (strcmp(device->d_name, VOLUME_NAME))
        return fail(ENOENT);
    if (NDevOpen(device))
        return fail(EBUSY);
    if (flags != O_RDONLY && flags != O_RDWR)
        return fail(EINVAL);
    device->d_private = calloc(1, sizeof(Cursor));
    if (!device->d_private)
        return fail(ENOMEM);
    if (flags == O_RDONLY)
        NDevSetReadOnly(device);
    NDevSetOpen(device);
    NDevSetBlock(device);
    return 0;
}

static int volume_sync(struct ntfs_device *device)
{
    if (!NDevOpen(device))
        return fail(EBADF);
    if (exchange(3, 0, 0, NULL, NULL))
        return -1;
    NDevClearDirty(device);
    return 0;
}

static int volume_close(struct ntfs_device *device)
{
    if (!NDevOpen(device))
        return fail(EBADF);
    int result = NDevDirty(device) ? volume_sync(device) : 0;
    free(device->d_private);
    device->d_private = NULL;
    NDevClearOpen(device);
    return result;
}

static s64 volume_seek(struct ntfs_device *device, s64 offset, int whence)
{
    Cursor *cursor = device->d_private;
    if (!cursor)
        return fail(EBADF);
    s64 base;
    switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = (s64)cursor->position; break;
    case SEEK_END: base = (s64)length; break;
    default: return fail(EINVAL);
    }
    if (offset < -base || offset > (s64)length - base)
        return fail(EINVAL);
    cursor->position = (uint64_t)(base + offset);
    return (s64)cursor->position;
}

static s64 transfer(struct ntfs_device *device, s64 count, s64 offset,
                    void *input, const void *output)
{
    if (!NDevOpen(device))
        return fail(EBADF);
    if (output && NDevReadOnly(device))
        return fail(EROFS);
    if (count < 0 || offset < 0 || (uint64_t)offset > length
            || (uint64_t)count > length - (uint64_t)offset)
        return fail(EINVAL);
    for (s64 done = 0; done < count;) {
        uint32_t chunk = (uint32_t)(count - done > MAX_TRANSFER
                         ? MAX_TRANSFER : count - done);
        if (exchange(output ? 2 : 1, offset + done, chunk,
                     input ? (unsigned char *)input + done : NULL,
                     output ? (const unsigned char *)output + done : NULL))
            return -1;
        done += chunk;
    }
    if (output && count)
        NDevSetDirty(device);
    return count;
}

static s64 volume_pread(struct ntfs_device *device, void *buffer, s64 count, s64 at)
{
    return transfer(device, count, at, buffer, NULL);
}

static s64 volume_pwrite(struct ntfs_device *device, const void *buffer, s64 count, s64 at)
{
    return transfer(device, count, at, NULL, buffer);
}

static s64 volume_read(struct ntfs_device *device, void *buffer, s64 count)
{
    Cursor *cursor = device->d_private;
    if (!cursor)
        return fail(EBADF);
    s64 result = volume_pread(device, buffer, count, (s64)cursor->position);
    if (result > 0)
        cursor->position += result;
    return result;
}

static s64 volume_write(struct ntfs_device *device, const void *buffer, s64 count)
{
    Cursor *cursor = device->d_private;
    if (!cursor)
        return fail(EBADF);
    s64 result = volume_pwrite(device, buffer, count, (s64)cursor->position);
    if (result > 0)
        cursor->position += result;
    return result;
}

static int volume_stat(struct ntfs_device *device, struct stat *status)
{
    (void)device;
    (void)status;
    return fail(ENOSYS);
}

static int volume_ioctl(struct ntfs_device *device, unsigned long request, void *output)
{
    if (!NDevOpen(device))
        return fail(EBADF);
    switch (request) {
    case BLKSSZGET:
        *(int *)output = (int)sector_size;
        return 0;
    case BLKBSZSET:
        return *(int *)output == (int)sector_size ? 0 : fail(EINVAL);
    case BLKGETSIZE64:
        *(uint64_t *)output = length;
        return 0;
    default:
        return fail(ENOTTY);
    }
}

struct ntfs_device_operations ntfs_device_default_io_ops = {
    .open = volume_open, .close = volume_close, .seek = volume_seek,
    .read = volume_read, .write = volume_write, .pread = volume_pread,
    .pwrite = volume_pwrite, .sync = volume_sync, .stat = volume_stat,
    .ioctl = volume_ioctl,
};

enum {
    CREATE_DIRECTORY = 1, CREATE_FILE = 2, LINK_FILE = 3,
    SET_METADATA = 4, WRITE_ROOT_STREAMS = 5, SET_SHORT_NAME = 6,
};

typedef struct {
    MFT_REF reference;
    int directory;
} Record;

static int receive_name(ntfschar *name, unsigned int units)
{
    if (units > 255)
        return fail(EINVAL);
    if (units && receive(name, units * sizeof(*name)))
        return -1;
    if ((units == 1 && name[0] == const_cpu_to_le16('.'))
            || (units == 2 && name[0] == const_cpu_to_le16('.')
                           && name[1] == const_cpu_to_le16('.'))
            || (units && ntfs_forbidden_chars(name, units, TRUE)))
        return fail(EINVAL);
    return 0;
}

static int write_stream_range(ntfs_attr *stream, uint64_t offset, uint64_t end)
{
    unsigned char block[65536];
    while (offset < end) {
        uint32_t bytes = (uint32_t)(end - offset > sizeof(block) ? sizeof(block) : end - offset);
        if (receive(block, bytes) || ntfs_attr_pwrite(stream, offset, bytes, block) != bytes)
            return -1;
        offset += bytes;
    }
    return 0;
}

static int make_nonresident(ntfs_attr *stream)
{
    ntfs_attr_search_ctx *ctx = ntfs_attr_get_search_ctx(stream->ni, NULL);
    if (!ctx)
        return -1;
    int result = ntfs_attr_lookup(AT_DATA, stream->name, stream->name_len,
                                  CASE_SENSITIVE, 0, NULL, 0, ctx);
    if (!result)
        result = ntfs_attr_make_non_resident(stream, ctx);
    ntfs_attr_put_search_ctx(ctx);
    return result;
}

static int write_sparse_stream(ntfs_attr *stream, uint64_t size)
{
    unsigned char count_bytes[4];
    if (receive(count_bytes, sizeof(count_bytes)))
        return -1;
    uint64_t count = get_le(count_bytes, sizeof(count_bytes));
    if (count > MAX_SPARSE_RANGES || !size)
        return fail(EINVAL);
    if (make_nonresident(stream) || ntfs_attr_truncate(stream, size))
        return -1;
    uint64_t previous_end = 0;
    uint64_t allocated = 0;
    for (uint64_t index = 0; index < count; index++) {
        unsigned char header[16];
        if (receive(header, sizeof(header)))
            return -1;
        uint64_t offset = get_le(header, 8);
        uint64_t bytes = get_le(header + 8, 8);
        if (offset < previous_end || offset >= size || !bytes || bytes > size - offset
                || offset % cluster_size || (bytes % cluster_size && offset + bytes != size))
            return fail(EINVAL);
        allocated += bytes;
        if (allocated > length)
            return fail(ENOSPC);
        previous_end = offset + bytes;
        if (write_stream_range(stream, offset, previous_end))
            return -1;
    }
    return 0;
}

static int write_stream(ntfs_inode *file, int *unnamed_allowed)
{
    unsigned char header[12];
    ntfschar name[255];
    if (receive(header, sizeof(header)))
        return -1;
    unsigned int units = (unsigned int)get_le(header, 2);
    uint16_t flags = (uint16_t)get_le(header + 2, 2);
    uint64_t size = get_le(header + 4, 8);
    int sparse = flags == le16_to_cpu(ATTR_IS_SPARSE);
    int compressed = flags == le16_to_cpu(ATTR_IS_COMPRESSED);
    int encrypted = flags == le16_to_cpu(ATTR_IS_ENCRYPTED);
    if ((flags && !sparse && !compressed && !encrypted) || (compressed && cluster_size > 4096)
            || (encrypted && (file->mrec->flags & MFT_RECORD_IS_DIRECTORY))
            || size > (sparse ? STREAM_LIMIT : length) || receive_name(name, units))
        return fail(EINVAL);
    uint64_t payload = encrypted && size ? (size + 511) & ~UINT64_C(511) : size;
    if (encrypted && size && (payload > length || length - payload < 2))
        return fail(EINVAL);
    if (!units) {
        if (!*unnamed_allowed)
            return fail(EINVAL);
        *unnamed_allowed = 0;
    }
    le32 saved_compression = file->flags & FILE_ATTR_COMPRESSED;
    file->flags &= ~FILE_ATTR_COMPRESSED;
    if (compressed)
        file->flags |= FILE_ATTR_COMPRESSED;
    int result = -1;
    if (units && ntfs_attr_add(file, AT_DATA, name, units, NULL, 0))
        goto restore_flags;
    ntfs_attr *stream = ntfs_attr_open(file, AT_DATA, units ? name : AT_UNNAMED, units);
    if (!stream)
        goto restore_flags;
    if (sparse)
        diskstudio_sparse_stream = stream;
    if (compressed && size && (make_nonresident(stream) || ntfs_attr_truncate(stream, 0)))
        goto close_stream;
    result = sparse ? write_sparse_stream(stream, size)
                    : write_stream_range(stream, 0, payload);
    if (!result && encrypted && size) {
        /* The documented ntfs-3g raw EFS import has a two-byte padding trailer.
         * The child derives it from the logical size and removes it on import. */
        le16 padding = cpu_to_le16((uint16_t)(payload - size));
        if (ntfs_attr_pwrite(stream, payload, sizeof(padding), &padding) != sizeof(padding))
            result = -1;
        payload += sizeof(padding);
    }
    if (!result)
        result = ntfs_attr_pclose(stream);
    if (!result && (stream->data_size != (s64)payload
            || le16_to_cpu(stream->data_flags) != (encrypted ? 0 : flags)))
        result = fail(EIO);
close_stream:
    diskstudio_sparse_stream = NULL;
    ntfs_attr_close(stream);
restore_flags:
    file->flags = (file->flags & ~FILE_ATTR_COMPRESSED) | saved_compression;
    return result;
}

static int restore_ea(ntfs_inode *file, unsigned int size)
{
    unsigned char info[8];
    if (receive(info, sizeof(info)) || get_le(info + 4, 4) != size)
        return fail(EINVAL);
    unsigned char *data = malloc(size);
    if (!data)
        return fail(ENOMEM);
    int result = -1;
    if (receive(data, size))
        goto done;
    unsigned int packed = 0, required = 0;
    for (unsigned int at = 0; at < size;) {
        const unsigned char *entry = data + at;
        unsigned int remaining = size - at;
        if (remaining < 9)
            goto invalid;
        unsigned int name = entry[5];
        unsigned int length = 9 + name + (unsigned int)get_le(entry + 6, 2);
        unsigned int padded = (length + 3) & ~3u;
        unsigned int next = (unsigned int)get_le(entry, 4);
        unsigned int span = next ? next : remaining;
        if ((entry[4] != 0 && entry[4] != 0x80) || !name || length > span
                || span > padded || span > remaining || (span < remaining && span != padded)
                || entry[8 + name])
            goto invalid;
        packed += length - 4;
        required += entry[4] == 0x80;
        at += span;
    }
    if (get_le(info, 2) != packed || get_le(info + 2, 2) != required)
        goto invalid;
    if (!ntfs_attr_add(file, AT_EA_INFORMATION, AT_UNNAMED, 0, info, sizeof(info))
            && !ntfs_attr_add(file, AT_EA, AT_UNNAMED, 0, data, size)) {
        NInoFileNameSetDirty(file);
        result = 0;
    }
    goto done;
invalid:
    errno = EINVAL;
done:
    free(data);
    return result;
}

static int restore_link_version_flag(ntfs_inode *file)
{
    ntfs_attr *info = ntfs_attr_open(file, AT_STANDARD_INFORMATION, AT_UNNAMED, 0);
    if (!info)
        return -1;
    const le32 value = cpu_to_le32(0x20);
    int result = 0;
    if (NAttrNonResident(info) || (info->data_size != 48 && info->data_size != 72)
            || ntfs_attr_pwrite(info, 40, sizeof(value), &value) != sizeof(value))
        result = fail(EIO);
    ntfs_attr_close(info);
    return result;
}

/* The WOF filter gets contents from the named stream. Set the unnamed
 * all-hole stream's valid data length without allocating any clusters. */
static int restore_wof_placeholder(ntfs_inode *file)
{
    static const ntfschar wof_name[] = {
        const_cpu_to_le16('W'), const_cpu_to_le16('o'), const_cpu_to_le16('f'),
        const_cpu_to_le16('C'), const_cpu_to_le16('o'), const_cpu_to_le16('m'),
        const_cpu_to_le16('p'), const_cpu_to_le16('r'), const_cpu_to_le16('e'),
        const_cpu_to_le16('s'), const_cpu_to_le16('s'), const_cpu_to_le16('e'),
        const_cpu_to_le16('d'), const_cpu_to_le16('D'), const_cpu_to_le16('a'),
        const_cpu_to_le16('t'), const_cpu_to_le16('a'),
    };
    ntfs_attr *backing = ntfs_attr_open(file, AT_DATA, wof_name, 17);
    if (!backing)
        return -1;
    int valid = !backing->data_flags && backing->data_size > 0
        && backing->initialized_size == backing->data_size;
    ntfs_attr_close(backing);
    if (!valid)
        return fail(EINVAL);

    ntfs_attr_search_ctx *ctx = ntfs_attr_get_search_ctx(file, NULL);
    if (!ctx)
        return -1;
    int result = ntfs_attr_lookup(AT_DATA, AT_UNNAMED, 0, CASE_SENSITIVE,
                                  0, NULL, 0, ctx);
    if (!result) {
        ATTR_RECORD *data = ctx->attr;
        if (!data->non_resident || data->flags != ATTR_IS_SPARSE
                || data->lowest_vcn || data->compressed_size
                || sle64_to_cpu(data->data_size) <= 0)
            result = fail(EINVAL);
        else {
            data->initialized_size = data->data_size;
            NInoSetDirty(ctx->ntfs_ino);
        }
    }
    ntfs_attr_put_search_ctx(ctx);
    return result;
}

static int restore_special_metadata(ntfs_inode *file, unsigned int special, uint32_t reparse_size)
{
    if (special & 1) {
        char identity[64];
        if (receive(identity, sizeof(identity))
                || ntfs_set_ntfs_object_id(file, identity, sizeof(identity), 0)) {
            ntfs_log_perror("Could not restore the NTFS object ID");
            return -1;
        }
    }
    if (reparse_size) {
        char reparse[REPARSE_LIMIT];
        if (receive(reparse, reparse_size))
            return -1;
        const unsigned char *bytes = (const unsigned char *)reparse;
        uint32_t tag = (uint32_t)get_le(bytes, 4);
        if (get_le(bytes + 4, 2) != reparse_size - 8)
            return fail(EINVAL);
        switch (tag) {
        case 0xa000000c: /* IO_REPARSE_TAG_SYMLINK */
            if (reparse_size < 20)
                return fail(EINVAL);
            break;
        case 0xa0000003: /* IO_REPARSE_TAG_MOUNT_POINT */
            if (reparse_size < 16 || !(file->mrec->flags & MFT_RECORD_IS_DIRECTORY))
                return fail(EINVAL);
            break;
        case 0x80000017: /* IO_REPARSE_TAG_WOF */
            if ((reparse_size != 24 && reparse_size != 28)
                    || (file->mrec->flags & MFT_RECORD_IS_DIRECTORY) || (special & 2)
                    || get_le(bytes + 6, 2) || get_le(bytes + 8, 4) != 1
                    || get_le(bytes + 12, 4) != 2 || get_le(bytes + 16, 4) != 1
                    || get_le(bytes + 20, 4) > 3
                    || (reparse_size == 28 && get_le(bytes + 24, 4)))
                return fail(EINVAL);
            if (restore_wof_placeholder(file))
                return -1;
            break;
        default:
            return fail(EINVAL);
        }
        if (ntfs_set_ntfs_reparse_data(file, reparse, reparse_size, 0)) {
            ntfs_log_perror("Could not restore the NTFS reparse point");
            return -1;
        }
        if ((special & 2) && restore_link_version_flag(file))
            return -1;
    }
    return 0;
}

static int restore_efs(ntfs_inode *file, uint32_t size)
{
    char *data = malloc(size);
    if (!data)
        return fail(ENOMEM);
    const unsigned char *bytes = (const unsigned char *)data;
    int result = receive(data, size);
    if (!result) {
        if (get_le(bytes, 4) != size || get_le(bytes + 4, 4)
                || get_le(bytes + 8, 4) != 2 || get_le(bytes + 72, 4))
            result = fail(EINVAL);
        else
            result = ntfs_set_efs_info(file, data, size, 0);
    }
    if (result)
        ntfs_log_perror("Could not restore the NTFS EFS utility stream");
    free(data);
    return result;
}

static int restore_metadata(ntfs_volume *volume, MFT_REF reference)
{
    unsigned char header[42];
    if (receive(header, sizeof(header)))
        return -1;
    uint32_t flags = (uint32_t)get_le(header, 4);
    uint32_t security_size = (uint32_t)get_le(header + 4, 4);
    uint32_t ea_size = (uint32_t)get_le(header + 32, 4);
    unsigned int index_compressed = header[36];
    unsigned int special = header[37];
    uint32_t reparse_size = (uint32_t)get_le(header + 38, 4);
    uint32_t settable = le32_to_cpu(FILE_ATTR_READONLY | FILE_ATTR_HIDDEN | FILE_ATTR_SYSTEM
                                    | FILE_ATTR_ARCHIVE | FILE_ATTR_TEMPORARY | FILE_ATTR_OFFLINE
                                    | FILE_ATTR_NOT_CONTENT_INDEXED | FILE_ATTR_NORMAL
                                    | FILE_ATTR_DIRECTORY | FILE_ATTR_I30_INDEX_PRESENT
                                    | FILE_ATTR_SPARSE_FILE | FILE_ATTR_COMPRESSED
                                    | FILE_ATTR_REPARSE_POINT | FILE_ATTR_ENCRYPTED);
    int encrypted = !!(flags & le32_to_cpu(FILE_ATTR_ENCRYPTED));
    if (ea_size)
        settable |= HAS_EXTENDED_ATTRIBUTES;
    if ((flags & ~settable) || security_size < 20 || security_size > 128 * 1024
            || ea_size > 65536 || index_compressed > 1 || special > 7
            || ((special & 4) && !encrypted)
            || (encrypted && (index_compressed || reparse_size
                || (flags & le32_to_cpu(FILE_ATTR_COMPRESSED | FILE_ATTR_SPARSE_FILE | FILE_ATTR_OFFLINE))))
            || reparse_size > REPARSE_LIMIT || (reparse_size && reparse_size < 8)
            || ((special & 2) && !reparse_size)
            || !!(flags & le32_to_cpu(FILE_ATTR_REPARSE_POINT)) != !!reparse_size)
        return fail(EINVAL);
    uint32_t efs_size = 0;
    if (special & 4) {
        unsigned char size[4];
        if (receive(size, sizeof(size)))
            return -1;
        efs_size = (uint32_t)get_le(size, sizeof(size));
        if (efs_size < 85 || efs_size > 256 * 1024)
            return fail(EINVAL);
    }
    char *security = malloc(security_size);
    if (!security)
        return fail(ENOMEM);
    int result = -1;
    if (receive(security, security_size))
        goto done;
    ntfs_inode *file = ntfs_inode_open(volume, reference);
    if (!file)
        goto done;
    struct SECURITY_CONTEXT context = {.vol = volume};
    int directory = !!(file->mrec->flags & MFT_RECORD_IS_DIRECTORY);
    if ((!directory && ((flags & le32_to_cpu(FILE_ATTR_I30_INDEX_PRESENT)) || index_compressed))
            || (!directory && encrypted && !efs_size)) {
        result = fail(EINVAL);
    } else if ((!index_compressed || !ntfs_attr_set_flags(file, AT_INDEX_ROOT,
                 NTFS_INDEX_I30, 4, ATTR_IS_COMPRESSED, ATTR_COMPRESSION_MASK))
            && (!ea_size || !restore_ea(file, ea_size))
            && !ntfs_set_ntfs_acl(&context, file, security, security_size, 0)
            && !restore_special_metadata(file, special, reparse_size)
            && (!efs_size || !restore_efs(file, efs_size))) {
        file->flags = cpu_to_le32(flags);
        NInoSetDirty(file);
        NInoFileNameSetDirty(file);
        result = ntfs_inode_set_times(file, (const char *)header + 8, 24, 0);
    }
    if (ntfs_inode_close(file))
        result = -1;
done:
    free(security);
    return result;
}

extern int diskstudio_link_dos_name(ntfs_inode *file, ntfs_inode *dir,
                                   const ntfschar *alias, unsigned int units);

static int set_short_name(ntfs_volume *volume, MFT_REF parent, MFT_REF source,
                          const ntfschar *alias, unsigned int units)
{
    ntfs_inode *directory = ntfs_inode_open(volume, parent);
    if (!directory)
        return -1;
    ntfs_inode *file = ntfs_inode_open(volume, source);
    int result = file ? diskstudio_link_dos_name(file, directory, alias, units) : -1;
    if (ntfs_inode_close(directory))
        result = -1;
    if (file && ntfs_inode_close(file))
        result = -1;
    return result;
}

static int materialize_record(ntfs_volume *volume, Record *records, uint32_t index)
{
    unsigned char header[12];
    ntfschar name[255];
    if (receive(header, sizeof(header)))
        return -1;
    unsigned int operation = header[0];
    unsigned int units = header[1];
    unsigned int streams = (unsigned int)get_le(header + 2, 2);
    uint32_t parent = (uint32_t)get_le(header + 4, 4);
    uint32_t source = (uint32_t)get_le(header + 8, 4);
    if (operation == SET_SHORT_NAME) {
        if (!units || units > 12 || streams || parent >= index || !records[parent].directory
                || !source || source >= index || source == parent || !records[source].reference
                || receive_name(name, units))
            return fail(EINVAL);
        return set_short_name(volume, records[parent].reference, records[source].reference,
                              name, units);
    }
    if (operation == SET_METADATA) {
        if (units || streams || parent || source >= index
                || !records[source].reference)
            return fail(EINVAL);
        return restore_metadata(volume, records[source].reference);
    }
    if (operation == WRITE_ROOT_STREAMS) {
        if (units || parent || source || !streams || streams > 4096)
            return fail(EINVAL);
        ntfs_inode *root = ntfs_inode_open(volume, records[0].reference);
        if (!root)
            return -1;
        int result = 0;
        int unnamed_allowed = 0;
        for (unsigned int stream = 0; stream < streams; stream++) {
            if (write_stream(root, &unnamed_allowed)) {
                result = -1;
                break;
            }
        }
        return ntfs_inode_close(root) ? -1 : result;
    }
    if (operation < CREATE_DIRECTORY || operation > LINK_FILE
            || parent >= index || !records[parent].directory
            || (operation == LINK_FILE
                ? (!source || source >= index || records[source].directory
                    || !records[source].reference || streams)
                : (source != 0 || streams > 4096))
            || !units || receive_name(name, units))
        return fail(EINVAL);
    ntfs_inode *directory = ntfs_inode_open(volume, records[parent].reference);
    if (!directory)
        return -1;
    ntfs_inode *file;
    int result = -1;
    if (operation == LINK_FILE)
        file = ntfs_inode_open(volume, records[source].reference);
    else
        file = ntfs_create(directory, const_cpu_to_le32(0), name, units,
                           operation == CREATE_DIRECTORY ? S_IFDIR : S_IFREG);
    if (!file)
        goto close_parent;
    if (operation == LINK_FILE) {
        result = ntfs_link(file, directory, name, units);
    } else {
        result = 0;
        int unnamed_allowed = operation == CREATE_FILE;
        for (unsigned int stream = 0; stream < streams; stream++) {
            if (write_stream(file, &unnamed_allowed)) {
                result = -1;
                break;
            }
        }
        records[index].reference = MK_MREF(file->mft_no,
                                           le16_to_cpu(file->mrec->sequence_number));
        records[index].directory = operation == CREATE_DIRECTORY;
    }
    if (ntfs_inode_close_in_dir(file, directory))
        result = -1;
close_parent:
    if (ntfs_inode_close(directory))
        result = -1;
    return result;
}

static int populate(void)
{
    unsigned char header[4];
    if (receive(header, sizeof(header)))
        return -1;
    uint32_t count = (uint32_t)get_le(header, 4);
    if (!count)
        return 0;
    if (count > 524289)
        return fail(EINVAL);

    Record *records = calloc((size_t)count + 1, sizeof(*records));
    if (!records)
        return fail(ENOMEM);
    /* A private library volume over the pipe. No Windows volume is mounted. */
    struct ntfs_device *device = ntfs_device_alloc(VOLUME_NAME, 0,
                                                   &ntfs_device_default_io_ops, NULL);
    if (!device) {
        free(records);
        return -1;
    }
    ntfs_volume *volume = ntfs_device_mount(device, NTFS_MNT_EXCLUSIVE);
    if (!volume) {
        ntfs_device_free(device);
        free(records);
        return -1;
    }
    records[0] = (Record){.reference = FILE_root, .directory = 1};
    int result = 0;
    for (uint32_t index = 1; index <= count; index++) {
        if (materialize_record(volume, records, index)) {
            ntfs_log_perror("Could not restore clone record %u", index);
            result = -1;
            break;
        }
    }
    if (ntfs_umount(volume, FALSE))
        result = -1;
    free(records);
    return result;
}

/* Supplied by the pinned, patched upstream source at build time. */
extern int diskstudio_mkntfs(int argc, char **argv);

int main(int argc, char **argv)
{
    unsigned char header[36];
    (void)argv;
    if (argc != 1 || _setmode(_fileno(stdin), _O_BINARY) < 0
            || _setmode(_fileno(stdout), _O_BINARY) < 0
            || fread(header, 1, sizeof(header), stdin) != sizeof(header)
            || memcmp(header, "DSN6", 4))
        return 2;

    sector_size = (uint32_t)get_le(header + 4, 4);
    length = get_le(header + 8, 8);
    start_lba = get_le(header + 16, 8);
    diskstudio_volume_serial = get_le(header + 24, 8);
    cluster_size = (uint32_t)get_le(header + 32, 4);
    if ((sector_size != 512 && sector_size != 4096) || length < 16 * 1024 * 1024
            || length > INT64_MAX || length % sector_size != 0 || start_lba > UINT32_MAX
            || diskstudio_volume_serial == 0 || cluster_size < sector_size
            || cluster_size > 65536 || (cluster_size & (cluster_size - 1)))
        return 2;

    char sector_argument[16], start_argument[16], count_argument[32], cluster_argument[16];
    snprintf(sector_argument, sizeof(sector_argument), "%u", sector_size);
    snprintf(start_argument, sizeof(start_argument), "%llu", (unsigned long long)start_lba);
    snprintf(count_argument, sizeof(count_argument), "%llu", (unsigned long long)(length / sector_size));
    snprintf(cluster_argument, sizeof(cluster_argument), "%u", cluster_size);
    char *parameters[] = {
        "diskstudio-ntfs", "-Q", "-F", "-L", "DISKCLONE", "-c", cluster_argument,
        "-s", sector_argument, "-p", start_argument, "-H", "255", "-S", "63",
        VOLUME_NAME, count_argument, NULL,
    };
    int result = diskstudio_mkntfs((int)(sizeof(parameters) / sizeof(parameters[0])) - 1,
                                  parameters);
    if (result || io_failed)
        return 1;
    if (populate() || io_failed) {
        ntfs_log_perror("Could not populate the NTFS target");
        return 1;
    }
    return 0;
}
