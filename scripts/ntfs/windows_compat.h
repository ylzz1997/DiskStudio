/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef DISKSTUDIO_NTFS_WINDOWS_COMPAT_H
#define DISKSTUDIO_NTFS_WINDOWS_COMPAT_H

#include <errno.h>
#include <stddef.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

/* The formatter and writer do not map host users, launch a daemon, or mount
 * anything. These compatibility definitions only allow the library to build. */
typedef unsigned int uid_t;
typedef unsigned int gid_t;
#define S_IFLNK 0120000
#define S_IFSOCK 0140000
#define S_ISUID 04000
#define S_ISGID 02000
#define S_ISVTX 01000
#define S_ISLNK(mode) (((mode) & S_IFMT) == S_IFLNK)
#define major(dev) (((dev) >> 8) & 255)
#define minor(dev) ((dev) & 255)
#define makedev(major, minor) (((major) << 8) | (minor))
#define ffs __builtin_ffs
#define random rand
#define srandom srand

struct passwd { uid_t pw_uid; char *pw_name; };
struct group { gid_t gr_gid; char **gr_mem; };
static inline uid_t getuid(void) { return 65534; }
static inline gid_t getgid(void) { return 65534; }
static inline struct passwd *getpwnam(const char *name)
{
    (void)name;
    errno = ENOSYS;
    return NULL;
}
static inline struct passwd *getpwuid(uid_t id)
{
    (void)id;
    errno = ENOSYS;
    return NULL;
}
static inline struct group *getgrnam(const char *name)
{
    (void)name;
    errno = ENOSYS;
    return NULL;
}
static inline struct group *getgrgid(gid_t id)
{
    (void)id;
    errno = ENOSYS;
    return NULL;
}

#endif
