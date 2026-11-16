# Encrypted backup envelope (implementation batch in progress)

This envelope protects an existing DiskStudio backup byte stream. UI, secret
handoff, immutable file selection, complete dependency-chain verification and
handle-relative output publication are connected. Consolidated checks and native
VM acceptance are recorded separately in `.vm/backup-protection-batch-status.json`;
implementation does not imply acceptance. It does not implement or claim
compatibility with DiskGenius proprietary backup encryption.

The primitives are RustCrypto [XChaCha20-Poly1305](https://docs.rs/chacha20poly1305/0.10.1/chacha20poly1305/)
and [Argon2id key derivation](https://docs.rs/argon2/0.5.3/argon2/). The KDF uses
64 MiB, three passes and four lanes, the memory-constrained recommendation in
[RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html#section-7.4).
The container construction and implementation are project code; a primitive's
audit is not evidence that this container has been independently audited.

All integers are little endian. The 4,096-byte header contains:

| Offset | Length | Meaning |
|---|---:|---|
| 0 | 8 | `DSBKENC1` |
| 8 | 4 | Plaintext frame size, exactly 1,048,576 |
| 12 | 4 | Argon2 version, exactly 0x13 |
| 16, 20, 24 | 4 each | Memory KiB 65536, passes 3, lanes 4 |
| 28 | 16 | OS-random salt |
| 44 | 16 | OS-random file identity |
| 60 | 4 | Reserved zero |
| 64 | 24 | OS-random header nonce |
| 88 | 32 | Encrypted metadata |
| 120 | 16 | Header authentication tag |
| 136 | 3960 | Reserved zero |

The encrypted metadata contains logical stream length, frame count, encoded frame
size (1,048,616), and the eight-byte `COMPLETE` marker. Unknown parameters are
rejected before allocating KDF memory. The derived key is 32 bytes. Passwords,
keys, KDF memory and plaintext caches use zeroizing ownership; this does not
promise erasure of OS swap, crash dumps or every compiler/library copy.

Each data record consists of a fresh 24-byte random nonce, exactly 1 MiB of
ciphertext, and a 16-byte tag. The last plaintext frame is zero padded. The
authenticated metadata gives its actual used length. Associated data contains
the immutable first 64 header bytes, a one-byte domain (header 0, data 1), and
an eight-byte index (header UINT64_MAX, data zero-based frame index).

Every frame rewrite generates a new nonce, including the archive header rewrite.
Records cannot be substituted across indices or files without authentication
failure. The writer allows no gaps, reopening, or post-finalization mutation.
Partial/failed writes poison the writer and are never retried. Drop performs no
flush. Ciphertext is flushed before the authenticated completion header, then
the final header is flushed. Incomplete outputs remain unpublished `.partial`
files; full decrypted archive-chain SHA-256 verification remains mandatory before
publication and before opening any writable restoration target.

Input files remain immutable retained handles. Exact physical length rejects
truncation and trailing data. A frame's tag is checked before returning plaintext;
authentication failures poison the reader. Opening an envelope validates only
the header, not every frame. The archive verification step must read the complete
logical content and validate all dependencies. It must never treat an opened
header as a successfully verified backup. Encryption adds at most one frame of
padding plus 40 bytes per frame and a 4 KiB outer header. No plaintext temporary
archive is created.

This does not detect an attacker replacing an entire archive with another valid
historical archive when no independent expected identity is available. Incremental
parents retain the existing manifest/identity binding and depth limit. Compression
and logical sizes may still be inferred approximately from ciphertext length.

The frontend sends only a typed encryption flag and compression preset. Passwords
are exchanged through one expiring, one-shot memory challenge; they do not appear
in a preview plan, history, settings, or command line. Cancelling a create challenge
ends the job before locking the source or creating output. Each encrypted parent
can have a different password. Cancellation or authentication failure aborts
opening; there is no plaintext fallback or automatic password retry.

Zstd presets are fast (level 1), balanced (3), and high (9), plus no compression.
Compression runs in bounded groups (up to four workers, two for high). The writer
uses the existing chunk hashes and only keeps a compressed chunk when it is smaller.
All retained parents, including superseded data, are verified before writable
restore handles are opened. Publication uses the exact retained file and directory
handles and never overwrites an existing final backup.
