/* DiskStudio adapter for e2fsprogs 1.47.4. SPDX-License-Identifier: LGPL-2.0-or-later
 * No device names are accepted. The parent supplies one already-verified handle
 * as stdin and a sector-aligned, bounded partition extent. No discard/TRIM.
 */
#include "config.h"
#include <windows.h>
#include <stdint.h>
#include <limits.h>
#include <io.h>
#include <fcntl.h>
#include <sys/stat.h>
#include "ext2fs.h"

struct ds_io {
    HANDLE handle;
    uint64_t base, length;
    unsigned sector;
    int writable;
    struct struct_io_stats stats;
};
static int ds_fd[2048];
/* Windows CRT argv uses the system code page. Labels cross this boundary as
 * ASCII hexadecimal UTF-8 bytes, preserving Chinese and other Unicode text. */
char *diskstudio_utf8_label(const char *hex) {
    static char label[17];
    size_t n = strlen(hex), i;
    if (!n || n > 32 || n % 2) exit(2);
    for (i=0; i<n; i+=2) {
        const char *digits="0123456789abcdef";
        const char *a=strchr(digits,hex[i]), *b=strchr(digits,hex[i+1]);
        if (!a || !b) exit(2);
        label[i/2]=(char)(((a-digits)<<4)|(b-digits));
        if ((unsigned char)label[i/2]<32 || label[i/2]==127) exit(2);
    }
    label[n/2]=0; return label;
}
static HANDLE ds_handle = INVALID_HANDLE_VALUE;
/* Reserve the capability before utilities redirect stdin. Interactive input
 * is always EOF; a utility can never interpret filesystem bytes as answers. */
__attribute__((constructor)) static void ds_initialize(void) {
    HANDLE original = GetStdHandle(STD_INPUT_HANDLE);
    if (original && original != INVALID_HANDLE_VALUE && GetFileType(original) == FILE_TYPE_DISK)
        DuplicateHandle(GetCurrentProcess(), original, GetCurrentProcess(), &ds_handle, 0, FALSE, DUPLICATE_SAME_ACCESS);
    freopen("NUL", "r", stdin);
}

static int number(const char *key, uint64_t *out) {
    const char *s = getenv(key);
    char *end;
    if (!s || !*s || *s == '-' || *s == '+') return EINVAL;
    errno = 0;
    *out = strtoull(s, &end, 10);
    return errno || *end ? EINVAL : 0;
}
static int parameters(struct ds_io *d) {
    uint64_t sector, write;
    memset(d, 0, sizeof(*d));
    if (number("DISKSTUDIO_EXT4_OFFSET", &d->base) ||
        number("DISKSTUDIO_EXT4_LENGTH", &d->length) ||
        number("DISKSTUDIO_EXT4_SECTOR", &sector) ||
        number("DISKSTUDIO_EXT4_WRITE", &write) || write > 1 ||
        (sector != 512 && sector != 4096) || d->length < 65536 ||
        d->length > INT64_MAX || d->base > INT64_MAX - d->length || d->base % sector || d->length % sector)
        return EINVAL;
    d->handle = ds_handle;
    if (!d->handle || d->handle == INVALID_HANDLE_VALUE ||
        GetFileType(d->handle) != FILE_TYPE_DISK) return EBADF;
    d->sector = (unsigned)sector;
    d->writable = (int)write;
    d->stats.num_fields = 2;
    return 0;
}
static int accepted(const char *name) { return name && !strcmp(name, "diskstudio"); }
static int exact(struct ds_io *d, uint64_t pos, void *data, DWORD count, int writing) {
    LARGE_INTEGER at;
    DWORD actual = 0;
    if (pos > d->length || count > d->length - pos || (writing && !d->writable))
        return EACCES;
    at.QuadPart = (LONGLONG)(d->base + pos);
    if (!SetFilePointerEx(d->handle, at, NULL, FILE_BEGIN)) return EIO;
    if (!(writing ? WriteFile(d->handle, data, count, &actual, NULL)
                  : ReadFile(d->handle, data, count, &actual, NULL)) || actual != count)
        return EIO;
    return 0;
}
static int transfer(struct ds_io *d, uint64_t pos, uint64_t bytes, void *data, int writing) {
    unsigned char bounce[4096];
    unsigned char *p = data;
    int ret;
    if (pos > d->length || bytes > d->length - pos || (writing && !d->writable))
        return EACCES;
    while (bytes) {
        unsigned in_sector = pos % d->sector;
        if (in_sector || bytes < d->sector) {
            unsigned n = d->sector - in_sector;
            if (n > bytes) n = (unsigned)bytes;
            ret = exact(d, pos - in_sector, bounce, d->sector, 0);
            if (ret) return ret;
            if (writing) {
                memcpy(bounce + in_sector, p, n);
                ret = exact(d, pos - in_sector, bounce, d->sector, 1);
                if (ret) return ret;
            } else memcpy(p, bounce + in_sector, n);
            pos += n; p += n; bytes -= n;
        } else {
            DWORD n = bytes > 1048576 ? 1048576 : (DWORD)(bytes / d->sector * d->sector);
            ret = exact(d, pos, p, n, writing);
            if (ret) return ret;
            pos += n; p += n; bytes -= n;
        }
    }
    return 0;
}
static errcode_t ds_open(const char *name, int flags, io_channel *out) {
    struct ds_io *d;
    io_channel c;
    int ret;
    if (!accepted(name)) return EACCES;
    d = calloc(1, sizeof(*d)); c = calloc(1, sizeof(*c));
    if (!d || !c) { free(d); free(c); return ENOMEM; }
    ret = parameters(d);
    if (!ret && (flags & IO_FLAG_RW) && !d->writable) ret = EACCES;
    if (ret) { free(d); free(c); return ret; }
    c->name = strdup(name);
    if (!c->name) { free(d); free(c); return ENOMEM; }
    c->magic = EXT2_ET_MAGIC_IO_CHANNEL;
    c->manager = windows_io_manager;
    c->private_data = d; c->block_size = 1024; c->refcount = 1;
    c->flags = CHANNEL_FLAGS_BLOCK_DEVICE | CHANNEL_FLAGS_NODISCARD;
    *out = c; return 0;
}
static errcode_t ds_flush(io_channel c) {
    struct ds_io *d = c->private_data;
    return !d->writable || FlushFileBuffers(d->handle) ? 0 : EIO;
}
static errcode_t ds_close(io_channel c) {
    errcode_t ret;
    if (--c->refcount > 0) return 0;
    ret = ds_flush(c);
    free(c->private_data); free(c->name); free(c);
    /* The inherited handle belongs to the process, never reopen a device. */
    return ret;
}
static errcode_t ds_blocksize(io_channel c, int size) {
    if (size < 512 || size > 65536 || (size & (size - 1))) return EINVAL;
    c->block_size = size; return 0;
}
static errcode_t blocks(io_channel c, uint64_t block, int count, void *p, int writing) {
    struct ds_io *d = c->private_data;
    uint64_t bytes = count < 0 ? -(int64_t)count : (uint64_t)count * c->block_size;
    if (block > UINT64_MAX / c->block_size) return EINVAL;
    int ret = transfer(d, block * c->block_size, bytes, p, writing);
    if (!ret) { if (writing) d->stats.bytes_written += bytes; else d->stats.bytes_read += bytes; }
    return ret;
}
static errcode_t ds_read64(io_channel c, unsigned long long b, int n, void *p) { return blocks(c,b,n,p,0); }
static errcode_t ds_write64(io_channel c, unsigned long long b, int n, const void *p) { return blocks(c,b,n,(void *)p,1); }
static errcode_t ds_read(io_channel c, unsigned long b, int n, void *p) { return ds_read64(c,b,n,p); }
static errcode_t ds_write(io_channel c, unsigned long b, int n, const void *p) { return ds_write64(c,b,n,p); }
static errcode_t ds_byte(io_channel c, unsigned long off, int n, const void *p) {
    if (n < 0) return EINVAL;
    return transfer(c->private_data, off, n, (void *)p, 1);
}
static errcode_t ds_option(io_channel c, const char *key, const char *value) { return EINVAL; }
static errcode_t ds_stats(io_channel c, io_stats *out) { if(out) *out=&((struct ds_io *)c->private_data)->stats; return 0; }
static errcode_t ds_discard(io_channel c, unsigned long long b, unsigned long long n) { return EXT2_ET_UNIMPLEMENTED; }
static errcode_t ds_readahead(io_channel c, unsigned long long b, unsigned long long n) { return 0; }
static errcode_t ds_zero(io_channel c, unsigned long long b, unsigned long long n) {
    struct ds_io *d = c->private_data;
    unsigned char *zero;
    uint64_t pos, bytes;
    int ret = 0;
    if (b > UINT64_MAX / c->block_size || n > UINT64_MAX / c->block_size) return EINVAL;
    pos = b * c->block_size; bytes = n * c->block_size;
    if (pos > d->length || bytes > d->length-pos || !d->writable) return EACCES;
    zero = calloc(1,1048576); if (!zero) return ENOMEM;
    while (bytes) {
        unsigned chunk = bytes > 1048576 ? 1048576 : (unsigned)bytes;
        ret = transfer(d,pos,chunk,zero,1); if(ret) break;
        pos += chunk; bytes -= chunk;
    }
    free(zero); return ret;
}
static struct struct_io_manager ds_manager = {
    .magic=EXT2_ET_MAGIC_IO_MANAGER, .name="DiskStudio bounded inherited handle",
    .open=ds_open,.close=ds_close,.set_blksize=ds_blocksize,
    .read_blk=ds_read,.write_blk=ds_write,.flush=ds_flush,.write_byte=ds_byte,
    .set_option=ds_option,.get_stats=ds_stats,.read_blk64=ds_read64,.write_blk64=ds_write64,
    .discard=ds_discard,.cache_readahead=ds_readahead,.zeroout=ds_zero,
};
io_manager windows_io_manager=&ds_manager;

/* Frontend utilities use these descriptors only for stat/size checks. Mark the
 * synthetic descriptor as a block device so resize2fs never truncates it. */
int ext2fs_open_file(const char *name, int flags, mode_t mode) {
    if (accepted(name)) {
        struct ds_io d; int fd;
        if (parameters(&d) || ((flags & (O_RDWR|O_WRONLY)) && !d.writable)) { errno=EACCES; return -1; }
        fd = _open("NUL", O_RDWR|O_BINARY);
        if (fd >= 2048) { _close(fd); errno=EMFILE; return -1; }
        if (fd >= 0) ds_fd[fd]=1;
        return fd;
    }
    errno=EACCES; return -1;
}
static int fake_stat(ext2fs_struct_stat *s) {
    struct ds_io d; int ret=parameters(&d);
    if(ret) { errno=ret; return -1; }
    memset(s,0,sizeof(*s)); s->st_mode=S_IFBLK|0600; s->st_size=d.length; s->st_nlink=1;
    return 0;
}
int ext2fs_stat(const char *name, ext2fs_struct_stat *s) {
    if(accepted(name)) return fake_stat(s);
    errno=EACCES; return -1;
}
int ext2fs_fstat(int fd, ext2fs_struct_stat *s) {
    if(fd>=0 && fd<2048 && ds_fd[fd]) return fake_stat(s);
    errno=EACCES; return -1;
}
errcode_t ext2fs_get_device_size2(const char *name, int blocksize, blk64_t *blocks) {
    struct ds_io d; int ret=parameters(&d);
    if(ret || !accepted(name) || blocksize<=0) return EINVAL;
    *blocks=d.length/blocksize; return 0;
}
errcode_t ext2fs_get_device_size(const char *name, int blocksize, blk_t *blocks) {
    blk64_t n; int ret=ext2fs_get_device_size2(name,blocksize,&n);
    if(ret) return ret; if(n>UINT32_MAX) return EOVERFLOW; *blocks=n; return 0;
}
errcode_t ext2fs_get_device_sectsize(const char *name, int *size) {
    struct ds_io d; if(!accepted(name)||parameters(&d)) return EINVAL;
    *size=d.sector; return 0;
}
errcode_t ext2fs_get_device_phys_sectsize(const char *name,int *size) { return ext2fs_get_device_sectsize(name,size); }
int ext2fs_get_dio_alignment(int fd) { (void)fd; return 4096; }
errcode_t ext2fs_check_mount_point(const char *name,int *flags,char *point,int len) {
    struct ds_io d; if(!accepted(name)||parameters(&d)) return EINVAL;
    *flags=0; if(point && len>0) point[0]=0; return 0;
}
errcode_t ext2fs_check_if_mounted(const char *name,int *flags) { return ext2fs_check_mount_point(name,flags,NULL,0); }
