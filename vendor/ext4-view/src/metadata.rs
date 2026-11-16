// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::file_type::FileType;
use crate::inode::InodeMode;
use crate::timestamp::Timestamp;

/// Metadata information about a file.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Metadata {
    /// DiskStudio: identity and validated inode record, for stale scan checks.
    pub(crate) inode_index: u32,
    pub(crate) inode_record: alloc::vec::Vec<u8>,
    /// Size in bytes of the file data.
    pub(crate) size_in_bytes: u64,

    /// Raw permissions and file type.
    pub(crate) mode: InodeMode,

    /// File type parsed from the `mode` bitfield.
    pub(crate) file_type: FileType,

    /// Owner user ID.
    pub(crate) uid: u32,

    /// Owner group ID.
    pub(crate) gid: u32,

    /// Last access time.
    pub(crate) atime: Timestamp,

    /// Last inode change time.
    pub(crate) ctime: Timestamp,

    /// Last data modification time.
    pub(crate) mtime: Timestamp,

    /// Creation time.
    /// Only present if inode is large enough.
    pub(crate) crtime: Option<Timestamp>,
}

impl Metadata {
    /// Filesystem-local inode number. This is not a host path or device handle.
    #[must_use]
    pub fn inode_number(&self) -> u32 {
        self.inode_index
    }

    /// Validated on-disk inode bytes, including generation and block mapping.
    #[must_use]
    pub fn inode_record(&self) -> &[u8] {
        &self.inode_record
    }
    /// Get the file type.
    #[must_use]
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Return true if this metadata is for a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    /// Return true if this metadata is for a symlink.
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    /// Get the size in bytes of the file.
    #[allow(clippy::len_without_is_empty)]
    #[must_use]
    pub fn len(&self) -> u64 {
        self.size_in_bytes
    }

    /// Get the file's UNIX permission bits.
    ///
    /// Diagram of the returned value's bits:
    ///
    /// ```text
    ///       top four bits are always zero
    ///       │   
    ///       │   set uid, set gid, sticky bit
    ///       │   │  
    ///       │   │  owner read/write/execute
    ///       │   │  │  
    ///       │   │  │  group read/write/execute
    ///       │   │  │  │  
    ///       │   │  │  │  other read/write/execute
    ///       │   │  │  │  │
    /// (msb) 0000xxxuuugggooo (lsb)
    /// ```
    ///
    /// See `st_mode` in [inode(7)][inode] for more details.
    ///
    /// [inode]: https://www.man7.org/linux/man-pages/man7/inode.7.html
    #[must_use]
    pub fn mode(&self) -> u16 {
        self.mode.bits() & 0o7777
    }

    /// Owner user ID.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// Owner group ID.
    #[must_use]
    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Last access time.
    #[must_use]
    pub fn accessed(&self) -> Timestamp {
        self.atime
    }

    /// Last data modification time.
    #[must_use]
    pub fn modified(&self) -> Timestamp {
        self.mtime
    }

    /// Creation time.
    #[must_use]
    pub fn created(&self) -> Option<Timestamp> {
        self.crtime
    }
}
