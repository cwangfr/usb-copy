use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MIB: usize = 1024 * 1024;
pub type Hash = [u8; 32];

#[derive(Clone, Copy)]
pub struct Settings {
    pub block_mib: usize,
    pub sync_mib: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            block_mib: 1,
            sync_mib: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Write,
    Sync,
    Verify,
}

pub fn copy_file(
    source: &Path,
    destination: &Path,
    settings: Settings,
    guard: impl Fn() -> io::Result<()>,
    mut checkpoint: impl FnMut(Phase, u64) -> io::Result<()>,
) -> io::Result<Hash> {
    if !(1..=16).contains(&settings.block_mib) || settings.sync_mib > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "无效的缓冲或同步设置",
        ));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    guard()?;
    checkpoint(Phase::Write, 0)?;
    if !is_regular(&fs::symlink_metadata(source)?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "源路径不是普通文件",
        ));
    }
    let mut input = open_read(source)?;
    let original = input.metadata()?;
    require_absent(destination)?;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stage = parent.join(format!(
        ".usb-copy-{}-{nonce:x}-{sequence:x}.part",
        std::process::id()
    ));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)?;
    let result = (|| {
        let mut output = output;
        let mut hash = Sha256::new();
        let block_size = (settings.block_mib * MIB) as u64;
        let mut buffer = vec![0; original.len().clamp(1, block_size) as usize];
        let sync_limit = (settings.sync_mib * MIB) as u64;
        let mut since_sync = 0;
        let mut written = 0;
        loop {
            let amount = buffer.len().min(if sync_limit == 0 {
                buffer.len()
            } else {
                (sync_limit - since_sync) as usize
            });
            let size = read_retry(&mut input, &mut buffer[..amount])?;
            if size == 0 {
                break;
            }
            output.write_all(&buffer[..size])?;
            hash.update(&buffer[..size]);
            written += size as u64;
            since_sync += size as u64;
            checkpoint(Phase::Write, written)?;
            if sync_limit != 0 && since_sync == sync_limit {
                checkpoint(Phase::Sync, written)?;
                output.sync_all()?;
                since_sync = 0;
                checkpoint(Phase::Write, written)?;
            }
        }
        let current = input.metadata()?;
        if original.len() != written
            || current.len() != original.len()
            || current.modified()? != original.modified()?
        {
            return Err(io::Error::other("复制时源文件发生变化"));
        }
        checkpoint(Phase::Sync, written)?;
        output.sync_all()?;
        drop(output);

        guard()?;
        checkpoint(Phase::Verify, 0)?;
        let expected: Hash = hash.finalize().into();
        let actual = read_hash(&stage, &mut buffer, |bytes| {
            checkpoint(Phase::Verify, bytes)
        })?;
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "目标文件 SHA-256 与源文件不一致",
            ));
        }
        guard()?;
        checkpoint(Phase::Verify, written)?;
        publish(&stage, destination)?;
        sync_directory(parent).map_err(|error| {
            io::Error::other(format!("文件已提交，但目标目录同步失败：{error}"))
        })?;
        Ok(expected)
    })();
    if let Err(error) = &result
        && guard().is_ok()
        && let Err(cleanup) = fs::remove_file(&stage)
        && cleanup.kind() != io::ErrorKind::NotFound
    {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "{error}；无法清理临时文件 {}：{cleanup}",
                stage
                    .file_name()
                    .unwrap_or(OsStr::new(""))
                    .to_string_lossy()
            ),
        ));
    }
    result
}

pub fn hash_file(path: &Path, mut progress: impl FnMut(u64) -> io::Result<()>) -> io::Result<Hash> {
    progress(0)?;
    let mut buffer = vec![0; MIB];
    read_hash(path, &mut buffer, progress)
}

fn read_hash(
    path: &Path,
    buffer: &mut [u8],
    mut progress: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<Hash> {
    let mut input = open_read(path)?;
    let original = input.metadata()?;
    let mut hash = Sha256::new();
    let mut total = 0;
    loop {
        let size = read_retry(&mut input, buffer)?;
        if size == 0 {
            let current = input.metadata()?;
            if total != original.len()
                || current.len() != original.len()
                || current.modified()? != original.modified()?
            {
                return Err(io::Error::other("回读时文件发生变化"));
            }
            return Ok(hash.finalize().into());
        }
        hash.update(&buffer[..size]);
        total += size as u64;
        progress(total)?;
    }
}

fn read_retry(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match file.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::OFlags;
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !is_regular(&file.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "路径不是普通文件",
        ));
    }
    Ok(file)
}

pub fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(target_os = "linux")]
    metadata.is_symlink()
}

fn is_regular(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !is_link(metadata)
}

fn require_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(windows)]
pub fn sync_directory(_path: &Path) -> io::Result<()> {
    // File data is flushed before publication. Windows has no portable directory fsync.
    Ok(())
}

#[cfg(target_os = "linux")]
fn publish(stage: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    Ok(renameat_with(
        CWD,
        stage,
        CWD,
        destination,
        RenameFlags::NOREPLACE,
    )?)
}

#[cfg(windows)]
fn publish(stage: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>()
    };
    let from = wide(stage);
    let to = wide(destination);
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn hex(hash: Hash) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(64);
    for byte in hash {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_multiple_blocks_and_verifies_known_sha256() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.bin");
        let target = dir.path().join("target.bin");
        fs::write(&source, vec![b'a'; MIB + 7])?;
        let mut phases = Vec::new();
        let hash = copy_file(
            &source,
            &target,
            Settings {
                block_mib: 1,
                sync_mib: 1,
            },
            || Ok(()),
            |phase, bytes| {
                phases.push((phase, bytes));
                Ok(())
            },
        )?;
        assert_eq!(fs::read(&source)?, fs::read(&target)?);
        assert_eq!(hash, hash_file(&target, |_| Ok(()))?);
        assert!(phases.contains(&(Phase::Sync, MIB as u64)));
        assert!(phases.contains(&(Phase::Sync, MIB as u64 + 7)));
        assert!(phases.contains(&(Phase::Verify, MIB as u64 + 7)));
        fs::write(&source, b"abc")?;
        assert_eq!(
            hex(hash_file(&source, |_| Ok(()))?),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        Ok(())
    }

    #[test]
    fn corrupted_stage_and_cancellation_never_publish() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, b"good")?;
        let error = copy_file(
            &source,
            &target,
            Settings::default(),
            || Ok(()),
            |phase, bytes| {
                if (phase, bytes) == (Phase::Verify, 0) {
                    let stage = fs::read_dir(dir.path())?
                        .flatten()
                        .find(|entry| entry.path().extension() == Some(OsStr::new("part")))
                        .unwrap();
                    fs::write(stage.path(), b"oops")?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = copy_file(
            &source,
            &target,
            Settings::default(),
            || Ok(()),
            |phase, bytes| {
                if phase == Phase::Write && bytes > 0 {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "取消"));
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn refuses_file_created_before_publication_and_handles_empty_file() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, [])?;
        let mut checks = 0;
        let error = copy_file(
            &source,
            &target,
            Settings::default(),
            || Ok(()),
            |phase, _| {
                if phase == Phase::Verify {
                    checks += 1;
                    if checks == 2 {
                        fs::write(&target, b"keep")?;
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target)?, b"keep");
        fs::remove_file(&target)?;
        let hash = copy_file(
            &source,
            &target,
            Settings::default(),
            || Ok(()),
            |_, _| Ok(()),
        )?;
        assert_eq!(
            hex(hash),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(fs::metadata(target)?.len(), 0);
        Ok(())
    }
}
