use super::*;
use crate::{
    image_browser::{Browser, Kind, Options},
    recovery::{self, ScanMode},
};
use std::sync::mpsc;
static TESTS: Mutex<()> = Mutex::new(());
fn frame(session: &str, request: &str, bytes: &[u8]) -> Vec<u8> {
    [
        uuid::Uuid::parse_str(session)
            .unwrap()
            .as_bytes()
            .as_slice(),
        uuid::Uuid::parse_str(request)
            .unwrap()
            .as_bytes()
            .as_slice(),
        bytes,
    ]
    .concat()
}
fn pattern(at: u64, len: usize) -> Vec<u8> {
    (at..at + len as u64)
        .map(|n| (n.wrapping_mul(31) >> 7) as u8)
        .collect()
}
#[test]
fn random_seeks_cache_short_reads_and_independent_contexts_match_exact_bytes() {
    let _guard = TESTS.lock().unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let recorded = requests.clone();
    let bridge = Bridge::new(2 * 1024 * 1024, move |event| {
        if let Event::Read {
            session,
            reader,
            request,
            offset,
            length,
        } = event
        {
            recorded.lock().unwrap().push((reader, offset, length));
            reply_bytes(&frame(&session, &request, &pattern(offset, length)))?;
        }
        Ok(())
    })
    .unwrap();
    let factory = Factory(bridge.clone());
    let mut a = factory.reader().unwrap();
    let mut b = factory.reader().unwrap();
    for (first, at, len) in [
        (true, 65530, 100003),
        (false, 13, 99),
        (true, 140000, 19),
        (false, 11, 15),
    ] {
        let reader = if first { &mut a } else { &mut b };
        reader.seek(SeekFrom::Start(at)).unwrap();
        let mut out = vec![0; len];
        reader.read_exact(&mut out).unwrap();
        assert_eq!(out, pattern(at, len));
    }
    assert!(requests.lock().unwrap().iter().all(|r| r.2 <= BLOCK));
    assert!(requests.lock().unwrap().len() < 8);
    assert!(a.seek(SeekFrom::Start(2 * 1024 * 1024 + 1)).is_err());
    assert!(b.seek(SeekFrom::Current(-999)).is_err());
    a.seek(SeekFrom::End(0)).unwrap();
    assert_eq!(a.read(&mut [0; 10]).unwrap(), 0);
    drop(a);
    drop(b);
    assert!(bridge.state.lock().unwrap().readers.is_empty());
}
#[test]
fn stale_ids_are_rejected_and_short_response_poisoning_cannot_turn_into_zeroes() {
    let _guard = TESTS.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let bridge = Bridge::new(1024 * 1024, move |e| tx.send(e).map_err(|e| e.to_string())).unwrap();
    let worker = Factory(bridge.clone()).reader().unwrap();
    let thread = std::thread::spawn(move || {
        let mut r = worker;
        let mut b = [99; 7];
        let e = r.read_exact(&mut b).unwrap_err();
        (e, b)
    });
    let Event::Read {
        session,
        request,
        length,
        ..
    } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!()
    };
    assert!(reply_bytes(&frame(&session, &uuid::Uuid::new_v4().to_string(), &[0; 7])).is_err());
    assert!(reply_bytes(&[0; 32]).is_err());
    assert!(reply_bytes(&vec![0; 33 + BLOCK]).is_err());
    reply_bytes(&frame(&session, &request, &vec![0; length - 1])).unwrap();
    let (error, bytes) = thread.join().unwrap();
    assert!(error.to_string().contains("长度"));
    assert_eq!(bytes, [99; 7]);
    assert!(bridge.healthy().is_err());
    assert!(Factory(bridge.clone()).reader().is_err());
    assert!(reply_bytes(&frame(&session, &request, &[0; 7])).is_err());
}
#[test]
fn cancel_unblocks_pending_read_but_new_operation_can_open_a_fresh_context() {
    let _guard = TESTS.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let bridge = Bridge::new(1024 * 1024, move |e| tx.send(e).map_err(|e| e.to_string())).unwrap();
    let mut r = Factory(bridge.clone()).reader().unwrap();
    let t = std::thread::spawn(move || r.read(&mut [0; 3]));
    let Event::Read {
        session, request, ..
    } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!()
    };
    bridge.cancel();
    assert!(t.join().unwrap().unwrap_err().to_string().contains("取消"));
    assert!(reply_error(&session, &request, "late".into()).is_err());
    assert!(bridge.healthy().is_ok());
    let mut next = Factory(bridge.clone()).reader().unwrap();
    let t = std::thread::spawn(move || {
        let mut b = [0; 3];
        next.read_exact(&mut b).unwrap();
        b
    });
    loop {
        if let Event::Read {
            session,
            request,
            offset,
            length,
            ..
        } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
        {
            reply_bytes(&frame(&session, &request, &pattern(offset, length))).unwrap();
            break;
        }
    }
    assert_eq!(t.join().unwrap(), pattern(0, 3).as_slice());
}
#[test]
fn timeout_and_callback_failure_are_terminal_and_reader_limits_are_bounded() {
    let _guard = TESTS.lock().unwrap();
    let bridge =
        Bridge::with_timeout(1024 * 1024, Arc::new(|_| Ok(())), Duration::from_millis(20)).unwrap();
    let mut readers: Vec<_> = (0..MAX_READERS)
        .map(|_| Factory(bridge.clone()).reader().unwrap())
        .collect();
    assert!(Factory(bridge.clone()).reader().is_err());
    let start = Instant::now();
    assert!(readers[0]
        .read(&mut [0; 1])
        .unwrap_err()
        .to_string()
        .contains("超时"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(bridge.healthy().is_err());
    drop(readers);
    drop(bridge);
    let bridge = Bridge::new(1024 * 1024, |_| Err("closed frontend".into())).unwrap();
    assert!(Factory(bridge.clone())
        .reader()
        .unwrap()
        .read(&mut [0; 1])
        .is_err());
    assert!(bridge.healthy().is_err());
}
#[test]
fn production_image_and_filesystem_readers_scan_and_extract_through_binary_frames() {
    let _guard = TESTS.lock().unwrap();
    let raw = &include_bytes!("../../tests/fixtures/ntfs-testfs1.img")[..];
    let bridge = Bridge::new(raw.len() as u64, move |event| {
        if let Event::Read {
            session,
            request,
            offset,
            length,
            ..
        } = event
        {
            reply_bytes(&frame(
                &session,
                &request,
                &raw[offset as usize..offset as usize + length],
            ))?;
        }
        Ok(())
    })
    .unwrap();
    let browser = Browser::from_factory(
        Arc::new(Factory(bridge.clone())),
        "synthetic.vdi".into(),
        Options {
            kind: Kind::Partition,
            sector: 512,
        },
        "VDI".into(),
        vec!["synthetic.vdi".into()],
        String::new(),
        || Ok(()),
    )
    .unwrap();
    let mut candidates = vec![];
    recovery::scan_filesystem_owned(
        browser.reader(0).unwrap(),
        ScanMode::AllFiles,
        Arc::new(AtomicBool::new(false)),
        |_, _| Ok(()),
        |c| candidates.push(c),
        |_| Ok(()),
    )
    .unwrap();
    let file = candidates
        .iter()
        .find(|c| c.name == "file-with-12345")
        .unwrap();
    let mut out = vec![];
    recovery::extract_owned(
        browser.reader(0).unwrap(),
        file,
        &mut out,
        file.size,
        Arc::new(AtomicBool::new(false)),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(out, b"12345");
    assert_eq!(browser.provenance(0).unwrap().format, "VDI");
    assert!(bridge.healthy().is_ok());
}
