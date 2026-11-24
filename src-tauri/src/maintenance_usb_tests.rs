use super::*;
use std::fs;

const BOOT: &[u8] = b"verified boot loader fixture";
const WIM: &[u8] = b"verified WIM fixture";

fn directory(path: &Path) -> Dir {
    Dir::open_ambient_dir(path, cap_std::ambient_authority()).unwrap()
}
fn fixture() -> (PathBuf, Dir, BTreeMap<String, Input>) {
    let parent =
        std::env::temp_dir().join(format!("diskstudio-winpe-usb-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&parent).unwrap();
    fs::create_dir(parent.join("target")).unwrap();
    let mut source = BTreeMap::new();
    for (name, value) in [(media::FALLBACK, BOOT), (media::WIM_PATH, WIM)] {
        let path = parent.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, value).unwrap();
        source.insert(
            name.into(),
            Input {
                file: File::open(&path).unwrap(),
                member: crate::maintenance_iso::Member {
                    path: name.into(),
                    size: value.len() as u32,
                },
            },
        );
    }
    let target = directory(&parent.join("target"));
    (parent, target, source)
}
fn check(_: &File) -> Result<(), String> {
    Ok(())
}
fn tick(_: &str, _: u64, _: u64) -> Result<(), String> {
    Ok(())
}

fn cleanup(path: PathBuf) {
    let resolved = path.canonicalize().unwrap();
    assert_eq!(
        resolved.parent(),
        Some(std::env::temp_dir().canonicalize().unwrap().as_path())
    );
    assert!(resolved
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("diskstudio-winpe-usb-"))
        .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok()));
    fs::remove_dir_all(resolved).unwrap();
}

#[test]
fn publishes_fallback_last_and_reuses_identical_files_without_touching_unrelated_paths() {
    let (parent, root, mut source) = fixture();
    root.create_dir("sources").unwrap();
    root.write(media::WIM_PATH, WIM).unwrap();
    root.write("photos.txt", b"existing file").unwrap();
    let preview = Contents::open(&root, &mut source, &check, &mut tick).unwrap();
    assert_eq!(preview.reused.len(), 1);
    assert!(!root.exists(media::FALLBACK));
    assert!(!root.exists("efi"));
    let mut actual = Contents::open(&root, &mut source, &check, &mut tick).unwrap();
    assert!(actual.layout == preview.layout);
    drop(preview);
    let mut staged = actual.stage(&mut source, &check, &mut tick).unwrap();
    assert!(!root.exists(media::FALLBACK));
    assert_eq!(staged.len(), 1);
    assert!(!publish(
        &actual,
        &mut staged,
        &root,
        &source[media::WIM_PATH],
        media::WIM_PATH,
        &check
    )
    .unwrap());
    assert!(publish(
        &actual,
        &mut staged,
        &root,
        &source[media::FALLBACK],
        media::FALLBACK,
        &check
    )
    .unwrap());
    actual
        .verify_published(
            &root,
            &source[media::FALLBACK],
            actual.layout.files[media::FALLBACK].as_deref().unwrap(),
            &check,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(root.read(media::FALLBACK).unwrap(), BOOT);
    assert_eq!(root.read("photos.txt").unwrap(), b"existing file");
    assert_eq!(root.read(media::WIM_PATH).unwrap(), WIM);
    assert!(staged.is_empty());
    drop((actual, root, source));
    cleanup(parent);
}

#[test]
fn refuses_different_existing_content_before_creating_any_directory_or_file() {
    let (parent, root, mut source) = fixture();
    root.create_dir("sources").unwrap();
    let different = vec![b'?'; WIM.len()];
    root.write(media::WIM_PATH, &different).unwrap();
    let error = Contents::open(&root, &mut source, &check, &mut tick)
        .err()
        .unwrap();
    assert!(error.contains(media::WIM_PATH));
    assert!(!root.exists("efi"));
    assert_eq!(root.read(media::WIM_PATH).unwrap(), different);
    drop((root, source));
    cleanup(parent);
}

#[test]
fn rechecks_new_paths_and_uses_no_replace_when_a_file_appears_after_staging() {
    let (parent, root, mut source) = fixture();
    let preview = Contents::open(&root, &mut source, &check, &mut tick).unwrap();
    root.create_dir("sources").unwrap();
    let mut current = Contents::open(&root, &mut source, &check, &mut tick).unwrap();
    assert!(preview.layout != current.layout);
    let mut staged = current.stage(&mut source, &check, &mut tick).unwrap();
    root.write(media::WIM_PATH, b"concurrent file").unwrap();
    assert!(publish(
        &current,
        &mut staged,
        &root,
        &source[media::WIM_PATH],
        media::WIM_PATH,
        &check
    )
    .is_err());
    assert_eq!(root.read(media::WIM_PATH).unwrap(), b"concurrent file");
    assert!(!root.exists(media::FALLBACK));
    drop((preview, current, root, source, staged));
    cleanup(parent);
}

#[test]
#[ignore = "requires prepared ordinary WinPE input files and an empty output directory"]
fn writes_real_winpe_boot_files_into_the_synthetic_usb_directory() {
    let source = PathBuf::from(std::env::var_os("DISKSTUDIO_WINPE_MEDIA_FIXTURE").unwrap());
    let target = PathBuf::from(std::env::var_os("DISKSTUDIO_WINPE_USB_FIXTURE").unwrap());
    let original = directory(&source);
    let root = directory(&target);
    assert_eq!(root.entries().unwrap().count(), 0);
    root.write("existing-user-file.txt", b"Retain this existing file.\r\n")
        .unwrap();
    root.create_dir("boot").unwrap();
    root.write(media::SDI_PATH, original.read(media::SDI_PATH).unwrap())
        .unwrap();
    let (mut files, mut preview) = scan_media(&original, &mut check).unwrap();
    if let Some(app) = std::env::var_os("DISKSTUDIO_WINPE_APP_FIXTURE") {
        attach_app(&mut files, &mut preview, File::open(app).unwrap()).unwrap();
    }
    let mut contents = Contents::open(&root, &mut files, &check, &mut tick).unwrap();
    assert_eq!(contents.reused.len(), 1);
    let mut staged = contents.stage(&mut files, &check, &mut tick).unwrap();
    assert_eq!(staged.len() + 1, preview.file_count);
    for (key, input) in files
        .iter()
        .filter(|(key, _)| key.as_str() != media::FALLBACK)
    {
        publish(&contents, &mut staged, &root, input, key, &check).unwrap();
        contents
            .verify_published(
                &root,
                input,
                contents.layout.files[key].as_deref().unwrap(),
                &check,
                |_| Ok(()),
            )
            .unwrap();
    }
    assert!(!root.exists(media::FALLBACK));
    let fallback = &files[media::FALLBACK];
    publish(
        &contents,
        &mut staged,
        &root,
        fallback,
        media::FALLBACK,
        &check,
    )
    .unwrap();
    contents
        .verify_published(
            &root,
            fallback,
            contents.layout.files[media::FALLBACK].as_deref().unwrap(),
            &check,
            |_| Ok(()),
        )
        .unwrap();
    assert!(staged.is_empty());
    assert_eq!(
        root.read("existing-user-file.txt").unwrap(),
        b"Retain this existing file.\r\n"
    );
    let recheck = Contents::open(&root, &mut files, &check, &mut tick).unwrap();
    assert_eq!(recheck.reused.len(), preview.file_count);
    if let Some(sha) = &preview.app_sha256 {
        assert_eq!(recheck.layout.files[media::APP_KEY].as_ref(), Some(sha));
    }
    assert_eq!(
        contents.digest(&files).unwrap(),
        recheck.digest(&files).unwrap()
    );
    println!(
        "WINPE USB files {} digest {}",
        preview.file_count,
        recheck.digest(&files).unwrap()
    );
}
