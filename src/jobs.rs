use crate::{
    copy::{self, Phase, Settings},
    volume::{self, Maintenance, Session, Volume},
};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Default)]
pub struct Monitor {
    pub cancel: AtomicBool,
    pub progress: Mutex<Progress>,
}

#[derive(Default, Clone)]
pub struct Progress {
    pub stage: &'static str,
    pub item: PathBuf,
    pub bytes: u64,
    pub size: u64,
    pub finished: usize,
    pub total: usize,
}

impl Monitor {
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "已取消"));
        }
        Ok(())
    }

    fn update(&self, stage: &'static str, item: &Path, bytes: u64, size: u64) -> io::Result<()> {
        self.check()?;
        let mut progress = self.progress.lock().unwrap();
        if progress.item != item {
            progress.item = item.to_owned();
        }
        progress.stage = stage;
        progress.bytes = bytes;
        progress.size = size;
        Ok(())
    }

    fn set_total(&self, total: usize) {
        self.progress.lock().unwrap().total = total;
    }

    fn advance(&self, count: usize) {
        self.progress.lock().unwrap().finished += count;
    }
}

pub enum Job {
    Copy {
        sources: Vec<PathBuf>,
        settings: Settings,
        rename: bool,
    },
    Delete(Vec<PathBuf>),
    Hash {
        names: Vec<PathBuf>,
        expected: Option<copy::Hash>,
    },
    Verify(PathBuf),
    Device(Maintenance),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Skipped,
    Failed,
}

pub struct ResultRow {
    pub path: PathBuf,
    pub outcome: Outcome,
    pub detail: String,
}

#[derive(Default)]
pub struct Report {
    pub rows: Vec<ResultRow>,
    pub hashes: Vec<(PathBuf, copy::Hash)>,
    pub cancelled: bool,
}

impl Report {
    fn row(&mut self, path: &Path, outcome: Outcome, detail: impl Into<String>) {
        self.rows.push(ResultRow {
            path: path.to_owned(),
            outcome,
            detail: detail.into(),
        });
    }
}

pub fn run(volume: &Volume, base: &Path, job: Job, monitor: &Monitor) -> Report {
    let mut report = Report::default();
    let result = (|| {
        if let Job::Device(operation) = job {
            monitor.update(operation.progress(), &volume.mount, 0, 0)?;
            report.row(&volume.mount, Outcome::Passed, volume.maintain(operation)?);
            return Ok(());
        }
        monitor.update("正在检查 U 盘", base, 0, 0)?;
        let session = Session::open(volume)?;
        session.guard()?;
        // Hold the selected directory for the lifetime of the operation.
        let _base = session.folder(base)?;
        match job {
            Job::Copy {
                sources,
                settings,
                rename,
            } => {
                let context = CopyContext {
                    session: &session,
                    base,
                    monitor,
                    settings,
                    rename,
                };
                context.run(sources, &mut report, volume::is_fat32(&volume.filesystem))
            }
            Job::Delete(names) => {
                monitor.set_total(names.len());
                for name in names {
                    let relative = base.join(&name);
                    session.guard()?;
                    monitor.check()?;
                    name_only(&name)?;
                    match remove(&session, &relative, monitor) {
                        Ok(()) => report.row(&name, Outcome::Passed, "已删除"),
                        Err(error) if stop(&error) => return Err(error),
                        Err(error) => report.row(&name, Outcome::Failed, error.to_string()),
                    }
                    monitor.advance(1);
                }
                Ok(())
            }
            Job::Hash { names, expected } => hash_items(
                &session,
                base,
                names.into_iter().map(|name| (name, expected)),
                monitor,
                &mut report,
            ),
            Job::Verify(manifest) => {
                monitor.update("正在读取校验清单", &manifest, 0, 0)?;
                let items = read_manifest(BufReader::new(copy::open_read(&manifest)?), || {
                    monitor.check()
                })?;
                hash_items(
                    &session,
                    base,
                    items.into_iter().map(|(name, hash)| (name, Some(hash))),
                    monitor,
                    &mut report,
                )
            }
            Job::Device(_) => unreachable!("handled before opening a file session"),
        }
    })();
    if let Err(error) = result {
        report.cancelled = error.kind() == io::ErrorKind::Interrupted;
        if !report.cancelled {
            report.row(base, Outcome::Failed, error.to_string());
        }
    }
    report
}

fn stop(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::NotConnected
    )
}

struct Source {
    path: PathBuf,
    size: u64,
    children: Option<Vec<Source>>,
    count: usize,
}

fn scan(
    path: PathBuf,
    fat32: bool,
    depth: usize,
    monitor: &Monitor,
    report: &mut Report,
) -> io::Result<Option<Source>> {
    monitor.update("正在扫描", &path, 0, 0)?;
    let result = (|| {
        if depth > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "目录嵌套超过 256 层",
            ));
        }
        let metadata = volume::checked_entry(&path)?;
        if metadata.is_file() {
            if fat32 && metadata.len() > u32::MAX.into() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "文件超过 FAT32 的 4 GiB − 1 字节上限",
                ));
            }
            return Ok(Source {
                path: path.clone(),
                size: metadata.len(),
                children: None,
                count: 1,
            });
        }
        let mut children = Vec::new();
        let mut size = 0u64;
        let mut count = 1;
        for entry in fs::read_dir(&path)? {
            if let Some(child) = scan(entry?.path(), fat32, depth + 1, monitor, report)? {
                size = size.saturating_add(child.size);
                count += child.count;
                children.push(child);
            }
        }
        children.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Source {
            path: path.clone(),
            size,
            children: Some(children),
            count,
        })
    })();
    match result {
        Ok(source) => Ok(Some(source)),
        Err(error) if stop(&error) => Err(error),
        Err(error) => {
            report.row(&path, Outcome::Failed, error.to_string());
            Ok(None)
        }
    }
}

struct CopyContext<'a> {
    session: &'a Session,
    base: &'a Path,
    monitor: &'a Monitor,
    settings: Settings,
    rename: bool,
}

impl CopyContext<'_> {
    fn run(&self, sources: Vec<PathBuf>, report: &mut Report, fat32: bool) -> io::Result<()> {
        let folder = self.session.folder(self.base)?;
        let target = fs::canonicalize(folder.path())?;
        let mut plan = Vec::new();
        let mut total_size = 0u64;
        let mut total_count = 0;
        for source in sources {
            self.monitor.check()?;
            let lookup = volume::checked_entry(&source).and_then(|metadata| {
                if metadata.is_dir() && target.starts_with(fs::canonicalize(&source)?) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "目标位于源目录内",
                    ));
                }
                let name = source.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "无法复制磁盘根目录")
                })?;
                let exists = exists(&folder.path().join(name))?;
                Ok(exists)
            });
            match lookup {
                Ok(exists) => {
                    if exists && !self.rename {
                        report.row(&source, Outcome::Skipped, "目标已有同名项目");
                    } else if let Some(node) = scan(source, fat32, 0, self.monitor, report)? {
                        total_size = total_size.saturating_add(node.size);
                        total_count += node.count;
                        plan.push(node);
                    }
                }
                Err(error) => report.row(&source, Outcome::Failed, error.to_string()),
            }
        }
        if total_size > self.session.available()? {
            return Err(io::Error::other(format!(
                "U 盘可用空间不足，需要 {total_size} 字节"
            )));
        }
        self.monitor.set_total(total_count);
        for node in plan {
            self.session.guard()?;
            self.monitor.check()?;
            let name = node.path.file_name().unwrap();
            let target_name = if self.rename {
                unused_name(folder.path(), name, node.children.is_some())?
            } else {
                name.to_owned()
            };
            self.copy_node(&node, &self.base.join(target_name), report)?;
        }
        Ok(())
    }

    fn copy_node(&self, node: &Source, relative: &Path, report: &mut Report) -> io::Result<()> {
        self.monitor.update("正在准备", relative, 0, node.size)?;
        self.session.guard()?;
        let name = relative.file_name().unwrap();
        let parent = self.session.folder(relative.parent().unwrap())?;
        let destination = parent.path().join(name);
        let local = relative.strip_prefix(self.base).unwrap();
        let result = if node.children.is_some() {
            fs::create_dir(&destination)
                .and_then(|()| copy::sync_directory(parent.path()))
                .map(|()| None)
        } else {
            copy::copy_file(
                &node.path,
                &destination,
                self.settings,
                || self.session.guard(),
                |phase, bytes| {
                    let stage = match phase {
                        Phase::Write => "正在写入",
                        Phase::Sync => "正在同步",
                        Phase::Verify => "正在回读校验",
                    };
                    self.monitor.update(stage, relative, bytes, node.size)
                },
            )
            .map(Some)
        };
        match result {
            Ok(hash) => {
                if let Some(hash) = hash {
                    report.hashes.push((local.to_owned(), hash));
                    report.row(relative, Outcome::Passed, copy::hex(hash));
                } else {
                    report.row(relative, Outcome::Passed, "目录已创建");
                }
                self.monitor.advance(1);
                if let Some(children) = &node.children {
                    for child in children {
                        self.copy_node(
                            child,
                            &relative.join(child.path.file_name().unwrap()),
                            report,
                        )?;
                    }
                }
            }
            Err(error) if stop(&error) => return Err(error),
            Err(error) => {
                self.session.guard()?;
                let outcome = if error.kind() == io::ErrorKind::AlreadyExists {
                    Outcome::Skipped
                } else {
                    Outcome::Failed
                };
                report.row(relative, outcome, error.to_string());
                self.monitor.advance(node.count);
            }
        }
        Ok(())
    }
}

fn name_only(path: &Path) -> io::Result<()> {
    if volume::parts(path)?.count() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "请选择 U 盘内的文件或文件夹",
        ));
    }
    Ok(())
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn unused_name(folder: &Path, name: &OsStr, directory: bool) -> io::Result<OsString> {
    if !exists(&folder.join(name))? {
        return Ok(name.to_owned());
    }
    let (stem, extension) = if directory {
        (name, None)
    } else {
        let name = Path::new(name);
        (
            name.file_stem().unwrap_or(name.as_os_str()),
            name.extension(),
        )
    };
    for number in 1..=10_000 {
        let mut candidate = stem.to_owned();
        candidate.push(format!(" ({number})"));
        if let Some(extension) = extension {
            candidate.push(".");
            candidate.push(extension);
        }
        if !exists(&folder.join(&candidate))? {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "无法分配可用的目标文件名",
    ))
}

fn remove(session: &Session, relative: &Path, monitor: &Monitor) -> io::Result<()> {
    monitor.update("正在删除", relative, 0, 0)?;
    if relative.components().take(257).count() > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "目录嵌套超过 256 层",
        ));
    }
    let parent = session.folder(relative.parent().unwrap())?;
    let path = parent.path().join(relative.file_name().unwrap());
    let metadata = volume::checked_entry(&path)?;
    if metadata.is_dir() {
        let folder = session.folder(relative)?;
        for entry in fs::read_dir(folder.path())? {
            let child = relative.join(entry?.file_name());
            remove(session, &child, monitor).map_err(|error| {
                io::Error::new(error.kind(), format!("{}：{error}", child.display()))
            })?;
        }
        drop(folder);
        session.guard()?;
        monitor.check()?;
        fs::remove_dir(&path)?;
    } else {
        session.guard()?;
        fs::remove_file(&path)?;
    }
    copy::sync_directory(parent.path())
}

fn hash_items(
    session: &Session,
    base: &Path,
    items: impl ExactSizeIterator<Item = (PathBuf, Option<copy::Hash>)>,
    monitor: &Monitor,
    report: &mut Report,
) -> io::Result<()> {
    monitor.set_total(items.len());
    for (name, expected) in items {
        monitor.check()?;
        session.guard()?;
        let result = (|| {
            let _ = volume::parts(&name)?;
            let filename = name
                .file_name()
                .ok_or_else(|| io::Error::other("缺少文件名"))?;
            let relative = base.join(&name);
            let folder = session.folder(relative.parent().unwrap())?;
            let path = folder.path().join(filename);
            let bytes = volume::checked_entry(&path)?.len();
            let hash =
                copy::hash_file(&path, |done| monitor.update("正在校验", &name, done, bytes))?;
            session.guard()?;
            Ok(hash)
        })();
        match result {
            Ok(actual) if expected.is_some_and(|expected| expected != actual) => {
                report.row(
                    &name,
                    Outcome::Failed,
                    format!(
                        "SHA-256 不匹配；实际 {}；预期 {}",
                        copy::hex(actual),
                        copy::hex(expected.unwrap()),
                    ),
                );
            }
            Ok(hash) => {
                report.hashes.push((name.clone(), hash));
                report.row(&name, Outcome::Passed, copy::hex(hash));
            }
            Err(error) if stop(&error) => return Err(error),
            Err(error) => report.row(&name, Outcome::Failed, error.to_string()),
        }
        monitor.advance(1);
    }
    Ok(())
}

pub fn parse_hash(input: &str) -> Option<copy::Hash> {
    let bytes = input.trim().as_bytes();
    if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut hash = [0; 32];
    let (pairs, _) = bytes.as_chunks::<2>();
    for (value, digits) in hash.iter_mut().zip(pairs) {
        *value = u8::from_str_radix(str::from_utf8(digits).ok()?, 16).ok()?;
    }
    Some(hash)
}

fn read_manifest(
    reader: impl BufRead,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<Vec<(PathBuf, copy::Hash)>> {
    let mut items = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        check()?;
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let error = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("校验清单第 {} 行无效", index + 1),
            )
        };
        let (line, escaped) = line
            .strip_prefix('\\')
            .map_or((line.as_str(), false), |line| (line, true));
        let hash = line.get(..64).and_then(parse_hash).ok_or_else(error)?;
        if !matches!(line.as_bytes().get(64..66), Some(b"  " | b" *")) {
            return Err(error());
        }
        let name = if escaped {
            unescape_name(&line[66..]).ok_or_else(error)?
        } else {
            line[66..].to_owned()
        };
        let path = PathBuf::from(&name);
        let _ = volume::parts(&path).map_err(|_| error())?;
        if name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            || name.contains('\0')
            || (cfg!(windows) && name.contains([':', '\\']))
        {
            return Err(error());
        }
        items.push((path, hash));
    }
    if items.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "校验清单为空"));
    }
    Ok(items)
}

fn unescape_name(name: &str) -> Option<String> {
    let mut result = String::new();
    let mut chars = name.chars();
    while let Some(character) = chars.next() {
        result.push(if character == '\\' {
            match chars.next()? {
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                _ => return None,
            }
        } else {
            character
        });
    }
    Some(result)
}

pub fn export(path: &Path, hashes: &[(PathBuf, copy::Hash)]) -> io::Result<()> {
    let mut data = String::new();
    for (name, hash) in hashes {
        let name = volume::parts(name)?
            .map(|part| {
                part.to_str().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "文件名无法表示为 UTF-8")
                })
            })
            .collect::<io::Result<Vec<_>>>()?
            .join("/");
        if name.contains(['\\', '\n', '\r']) {
            data.push('\\');
        }
        data.push_str(&copy::hex(*hash));
        data.push_str("  ");
        data.push_str(
            &name
                .replace('\\', "\\\\")
                .replace('\n', "\\n")
                .replace('\r', "\\r"),
        );
        data.push('\n');
    }
    let mut output = OpenOptions::new().create_new(true).write(true).open(path)?;
    let result = output
        .write_all(data.as_bytes())
        .and_then(|()| output.sync_all());
    drop(output);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

pub fn temp_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with(".usb-copy-") && name.ends_with(".part")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "copies and re-reads a file over 4 GiB"]
    fn exfat_policy_allows_and_verifies_a_file_over_four_gib() -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        let disk = tempfile::tempdir()?;
        let local = tempfile::tempdir()?;
        let mut volume = Volume::test_volume(disk.path())?;
        let source = local.path().join("large.bin");
        let end = 1u64 << 32;
        let mut input = File::create(&source)?;
        input.seek(SeekFrom::Start(end))?;
        input.write_all(b"verified beyond four GiB")?;
        drop(input);
        let copy = |volume: &Volume| {
            run(
                volume,
                Path::new(""),
                Job::Copy {
                    sources: vec![source.clone()],
                    settings: Settings {
                        block_mib: 8,
                        sync_mib: 0,
                    },
                    rename: false,
                },
                &Monitor::default(),
            )
        };
        volume.filesystem = "vfat".to_owned();
        assert_eq!(copy(&volume).rows[0].outcome, Outcome::Failed);
        assert!(!disk.path().join("large.bin").exists());

        volume.filesystem = "exfat".to_owned();
        let report = copy(&volume);
        assert_eq!(
            report.rows[0].outcome,
            Outcome::Passed,
            "{}",
            report.rows[0].detail
        );
        assert_eq!(report.hashes.len(), 1);
        let mut output = File::open(disk.path().join("large.bin"))?;
        assert_eq!(output.metadata()?.len(), fs::metadata(&source)?.len());
        output.seek(SeekFrom::Start(end))?;
        let mut tail = Vec::new();
        output.read_to_end(&mut tail)?;
        assert_eq!(tail, b"verified beyond four GiB");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copies_directory_verifies_manifest_and_deletes_inside_volume() -> io::Result<()> {
        let disk = tempfile::tempdir()?;
        let local = tempfile::tempdir()?;
        let volume = Volume::test_volume(disk.path())?;
        let source = local.path().join("资料");
        let backup = Path::new("备份");
        fs::create_dir(disk.path().join(backup))?;
        fs::create_dir_all(source.join("空目录"))?;
        fs::write(source.join("文件.txt"), b"copy this")?;
        let run_copy = |rename| {
            run(
                &volume,
                backup,
                Job::Copy {
                    sources: vec![source.clone()],
                    settings: Settings::default(),
                    rename,
                },
                &Monitor::default(),
            )
        };
        let copied = run_copy(false);
        assert!(
            copied
                .rows
                .iter()
                .all(|item| item.outcome == Outcome::Passed)
        );
        assert_eq!(copied.hashes.len(), 1);
        assert!(disk.path().join("备份/资料/空目录").is_dir());
        assert_eq!(run_copy(false).rows[0].outcome, Outcome::Skipped);
        assert_eq!(run_copy(true).hashes[0].0, Path::new("资料 (1)/文件.txt"));

        let manifest = local.path().join("backup.sha256");
        export(&manifest, &copied.hashes)?;
        let checked = run(
            &volume,
            backup,
            Job::Verify(manifest.clone()),
            &Monitor::default(),
        );
        assert_eq!(checked.hashes, copied.hashes);
        fs::write(disk.path().join("备份/资料/文件.txt"), b"changed")?;
        let checked = run(&volume, backup, Job::Verify(manifest), &Monitor::default());
        assert!(
            checked
                .rows
                .iter()
                .any(|item| item.outcome == Outcome::Failed)
        );

        std::os::unix::fs::symlink(&source, disk.path().join("备份/link"))?;
        let deleted = run(
            &volume,
            backup,
            Job::Delete(vec![PathBuf::from("link"), PathBuf::from("资料")]),
            &Monitor::default(),
        );
        assert_eq!(deleted.rows.len(), 2);
        assert!(deleted.rows[0].outcome == Outcome::Failed);
        assert!(deleted.rows[1].outcome == Outcome::Passed);
        assert!(source.join("文件.txt").is_file());
        assert!(!disk.path().join("备份/资料").exists());
        Ok(())
    }

    #[test]
    fn manifest_round_trip_and_rejects_outside_paths() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let manifest = directory.path().join("sample.sha256");
        let items = vec![(PathBuf::from("资料/镜像.img"), [42; 32])];
        export(&manifest, &items)?;
        assert_eq!(
            read_manifest(BufReader::new(File::open(&manifest)?), || Ok(()))?,
            items
        );
        assert_eq!(
            export(&manifest, &items).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        for name in ["../elsewhere", "/absolute", "inside/../also-unsafe", ""] {
            let line = format!("{}  {name}\n", copy::hex([0; 32]));
            assert!(read_manifest(line.as_bytes(), || Ok(())).is_err());
        }
        Ok(())
    }
}
