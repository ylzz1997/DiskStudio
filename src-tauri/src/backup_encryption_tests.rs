use super::*;
use std::io::Cursor;

fn writer() -> Writer<Cursor<Vec<u8>>> {
    Writer::with_key(
        Cursor::new(vec![]),
        make_prefix().unwrap(),
        Zeroizing::new([0x71; 32]),
    )
}
fn open(bytes: Vec<u8>, key: [u8; 32]) -> io::Result<Reader<Cursor<Vec<u8>>>> {
    let mut file = Cursor::new(bytes);
    let header = read_header(&mut file)?;
    Reader::from_header(file, header, Zeroizing::new(key))
}
fn encrypted(data: &[u8]) -> Vec<u8> {
    let mut out = writer();
    out.reset().unwrap();
    out.write_all(data).unwrap();
    out.sync().unwrap();
    out.into_inner().unwrap().into_inner()
}

#[test]
fn public_password_roundtrip_wrong_password_and_bounded_parameters() {
    let password = Password::new("test-only synthetic password".into()).unwrap();
    let mut out = Writer::new(Cursor::new(vec![]), &password).unwrap();
    out.reset().unwrap();
    out.write_all(b"confidential synthetic bytes").unwrap();
    out.sync().unwrap();
    let bytes = out.into_inner().unwrap().into_inner();
    let mut read = Reader::open(Cursor::new(bytes.clone()), &password).unwrap();
    let mut clear = vec![];
    read.read_to_end(&mut clear).unwrap();
    assert_eq!(clear, b"confidential synthetic bytes");
    assert!(Reader::open(
        Cursor::new(bytes.clone()),
        &Password::new("wrong synthetic password".into()).unwrap()
    )
    .is_err());
    let mut bad = bytes;
    bad[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Reader::open(Cursor::new(bad), &password)
        .unwrap_err_text()
        .contains("未派生密钥"));
    assert!(Writer::new(Cursor::new(vec![9]), &password).is_err());
    assert!(Writer::new(Cursor::new(vec![]), &Password::new("short".into()).unwrap()).is_err());
}
// Result::unwrap_err requires Debug for the secret-bearing successful type.
trait ErrorText<T> {
    fn unwrap_err_text(self) -> String;
}
impl<T> ErrorText<T> for io::Result<T> {
    fn unwrap_err_text(self) -> String {
        match self {
            Ok(_) => panic!("expected failure"),
            Err(e) => e.to_string(),
        }
    }
}

#[test]
fn random_access_rewrite_changes_nonce_and_preserves_every_byte() {
    let mut plain = (0..BLOCK * 2 + 31)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let mut out = writer();
    out.reset().unwrap();
    out.write_all(&plain).unwrap();
    out.flush().unwrap();
    let old_nonce = out.inner.get_ref()[HEADER..HEADER + 24].to_vec();
    out.seek(SeekFrom::Start(0)).unwrap();
    out.write_all(b"REWRITTEN").unwrap();
    plain[..9].copy_from_slice(b"REWRITTEN");
    out.flush().unwrap();
    assert_ne!(&out.inner.get_ref()[HEADER..HEADER + 24], &old_nonce);
    // A boundary-spanning overwrite must load and authenticate both old records.
    out.seek(SeekFrom::Start(BLOCK as u64 - 7)).unwrap();
    out.write_all(&[3; 19]).unwrap();
    plain[BLOCK - 7..BLOCK + 12].fill(3);
    out.sync().unwrap();
    assert!(out.write(b"x").is_err());
    assert!(out.reset().is_err());
    let bytes = out.into_inner().unwrap().into_inner();
    assert_eq!(
        bytes.len() as u64,
        storage_bound(plain.len() as u64).unwrap()
    );
    assert!(!bytes.windows(9).any(|b| b == b"REWRITTEN"));
    let mut read = open(bytes, [0x71; 32]).unwrap();
    let mut actual = vec![];
    read.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, plain);
    read.seek(SeekFrom::End(-31)).unwrap();
    let mut tail = vec![];
    read.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, &plain[plain.len() - 31..]);
    assert!(read.seek(SeekFrom::End(1)).is_err());
}

#[test]
fn corruption_reordering_cross_file_splicing_truncation_and_append_are_rejected() {
    let bytes = encrypted(&vec![0x63; BLOCK + 19]);
    for offset in [
        64,
        88,
        120,
        HEADER,
        HEADER + 32,
        HEADER + RECORD - 1,
        HEADER + RECORD + 24,
    ] {
        let mut bad = bytes.clone();
        bad[offset] ^= 1;
        match open(bad, [0x71; 32]) {
            Err(_) => {}
            Ok(mut r) => {
                assert!(r.read_to_end(&mut vec![]).is_err());
                assert!(r.seek(SeekFrom::Start(0)).is_err());
            }
        }
    }
    let mut swapped = bytes.clone();
    let first = swapped[HEADER..HEADER + RECORD].to_vec();
    swapped.copy_within(HEADER + RECORD..HEADER + 2 * RECORD, HEADER);
    swapped[HEADER + RECORD..].copy_from_slice(&first);
    assert!(open(swapped, [0x71; 32])
        .unwrap()
        .read_to_end(&mut vec![])
        .is_err());
    let foreign = encrypted(&vec![0x63; BLOCK + 19]);
    let mut splice = bytes.clone();
    splice[HEADER..HEADER + RECORD].copy_from_slice(&foreign[HEADER..HEADER + RECORD]);
    assert!(open(splice, [0x71; 32])
        .unwrap()
        .read_to_end(&mut vec![])
        .is_err());
    let mut short = bytes.clone();
    short.pop();
    assert!(open(short, [0x71; 32]).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(open(extra, [0x71; 32]).is_err());
    let mut unfinished = bytes;
    unfinished[64..136].fill(0);
    assert!(open(unfinished, [0x71; 32]).is_err());
}

#[test]
fn header_not_published_before_sync_and_no_plaintext_drop_flush() {
    let mut out = writer();
    out.reset().unwrap();
    out.write_all(b"synthetic secret").unwrap();
    out.flush().unwrap();
    assert!(open(out.inner.get_ref().clone(), [0x71; 32]).is_err());
    assert!(!out
        .inner
        .get_ref()
        .windows(16)
        .any(|b| b == b"synthetic secret"));
    assert!(out.into_inner().is_err());
    assert!(storage_bound(0).is_err());
    assert!(storage_bound(MAX_SIZE + 1).is_err());
    assert_eq!(storage_bound(1).unwrap(), (HEADER + RECORD) as u64);
}

struct Fault {
    data: Cursor<Vec<u8>>,
    writes: usize,
    short_at: usize,
    sync_error: bool,
}
impl Read for Fault {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.data.read(b)
    }
}
impl Seek for Fault {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.data.seek(p)
    }
}
impl Write for Fault {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.writes == self.short_at {
            self.data.write(&b[..b.len() / 2])
        } else {
            self.data.write(b)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Output for Fault {
    fn reset(&mut self) -> io::Result<()> {
        self.data.get_mut().clear();
        self.data.set_position(0);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        if self.sync_error {
            Err(error("injected sync failure"))
        } else {
            Ok(())
        }
    }
}
#[test]
fn short_write_and_flush_failure_poison_output_without_retry() {
    let file = Fault {
        data: Cursor::new(vec![]),
        writes: 0,
        short_at: 2,
        sync_error: false,
    };
    let mut out = Writer::with_key(file, make_prefix().unwrap(), Zeroizing::new([0x71; 32]));
    out.reset().unwrap();
    out.write_all(&[1; 32]).unwrap();
    assert!(out.flush().is_err());
    let writes = out.inner.writes;
    assert!(out.flush().is_err());
    assert!(out.sync().is_err());
    assert!(out.write(b"x").is_err());
    assert_eq!(out.inner.writes, writes);
    assert!(out.into_inner().is_err());
    let file = Fault {
        data: Cursor::new(vec![]),
        writes: 0,
        short_at: usize::MAX,
        sync_error: true,
    };
    let mut out = Writer::with_key(file, make_prefix().unwrap(), Zeroizing::new([0x71; 32]));
    assert!(out.reset().is_err());
    let writes = out.inner.writes;
    assert!(out.reset().is_err());
    assert_eq!(out.inner.writes, writes);
}

#[test]
fn existing_archive_full_incremental_and_used_space_can_be_encrypted_without_plain_temp() {
    use crate::backup_archive::{self as archive, AllocationPolicy, Archive, Chain};
    let plain = vec![9; 512 * 1024];
    let mut out = writer();
    let header = archive::create(
        &mut plain.as_slice(),
        &mut out,
        "partition",
        plain.len() as u64,
        512,
        true,
        None,
        |_| Ok(()),
    )
    .unwrap();
    let base = out.into_inner().unwrap().into_inner();
    let parent = Chain::new(Archive::from_reader(open(base.clone(), [0x71; 32]).unwrap()).unwrap());
    let mut child = writer();
    let mut changed = plain.clone();
    changed[123] = 7;
    let child_header = archive::create_with_policy(
        &mut changed.as_slice(),
        &mut child,
        "partition",
        changed.len() as u64,
        512,
        true,
        Some(&parent),
        Some(AllocationPolicy {
            skipped_bytes: 0,
            map_sha256: "a".repeat(64),
        }),
        |_| Ok(()),
    )
    .unwrap();
    assert_ne!(header.sha256, child_header.sha256);
    let mut chain = Chain::new(Archive::from_reader(child.into_reader().unwrap()).unwrap());
    chain
        .add_parent(Archive::from_reader(open(base, [0x71; 32]).unwrap()).unwrap())
        .unwrap();
    chain.complete().unwrap();
    chain.verify(|_| Ok(())).unwrap();
    let mut actual = vec![];
    chain.reader().read_to_end(&mut actual).unwrap();
    assert_eq!(actual, changed);
}
