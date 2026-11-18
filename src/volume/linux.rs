use super::{Folder, changed, parts};
use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, fstatvfs, openat, openat2};
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStringExt,
            fs::{FileTypeExt, MetadataExt},
        },
    },
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub mount: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub available: u64,
    mount_id: u64,
}

struct Mount {
    id: u64,
    device: String,
    root: PathBuf,
    path: PathBuf,
    filesystem: String,
    source: PathBuf,
}

fn unescape(value: &[u8]) -> OsString {
    let mut output = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\'
            && let Some(digits) = value.get(index + 1..index + 4)
            && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        {
            let octal = u8::from_str_radix(std::str::from_utf8(digits).unwrap(), 8);
            if let Ok(byte) = octal {
                output.push(byte);
                index += 4;
                continue;
            }
        }
        output.push(value[index]);
        index += 1;
    }
    OsString::from_vec(output)
}

fn mounts() -> io::Result<Vec<Mount>> {
    let data = fs::read("/proc/self/mountinfo")?;
    Ok(data
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let fields: Vec<_> = line.split(|&byte| byte == b' ').collect();
            let separator = fields.iter().position(|&field| field == b"-")?;
            Some(Mount {
                id: std::str::from_utf8(fields.first()?).ok()?.parse().ok()?,
                device: String::from_utf8_lossy(fields.get(2)?).into_owned(),
                root: unescape(fields.get(3)?).into(),
                path: unescape(fields.get(4)?).into(),
                filesystem: String::from_utf8_lossy(fields.get(separator + 1)?).into_owned(),
                source: unescape(fields.get(separator + 2)?).into(),
            })
        })
        .collect())
}

fn usb_device(device: &str) -> Option<PathBuf> {
    let sys = fs::canonicalize(Path::new("/sys/dev/block").join(device)).ok()?;
    let is_usb = sys.ancestors().any(|ancestor| {
        fs::read_link(ancestor.join("subsystem")).is_ok_and(|subsystem| subsystem.ends_with("usb"))
    });
    is_usb.then_some(sys)
}

fn mount_device(mount: &Mount) -> Option<PathBuf> {
    usb_device(&mount.device).or_else(|| {
        // FUSE-backed block filesystems use a virtual major/minor in mountinfo.
        if mount.filesystem != "fuseblk" || !mount.source.starts_with("/dev") {
            return None;
        }
        let metadata = fs::metadata(&mount.source).ok()?;
        if !metadata.file_type().is_block_device() {
            return None;
        }
        let device = metadata.rdev();
        usb_device(&format!(
            "{}:{}",
            rustix::fs::major(device),
            rustix::fs::minor(device)
        ))
    })
}

pub fn detect() -> io::Result<Vec<Volume>> {
    let mut result = Vec::new();
    for mount in mounts()? {
        if mount.root != Path::new("/")
            || mount.path == Path::new("/")
            || mount.path.starts_with("/boot")
        {
            continue;
        }
        let Some(device) = mount_device(&mount) else {
            continue;
        };
        let Some(name) = device.file_name() else {
            continue;
        };
        let label = fs::read_dir("/dev/disk/by-label")
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|entry| {
                let link = fs::read_link(entry.path()).ok()?;
                (link.file_name()? == name)
                    .then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| name.to_string_lossy().into_owned());
        let mut volume = Volume {
            mount: mount.path,
            label,
            filesystem: mount.filesystem,
            available: 0,
            mount_id: mount.id,
        };
        if let Ok(session) = Session::open(&volume)
            && let Ok(available) = session.available()
        {
            volume.available = available;
            result.push(volume);
        }
    }
    result.sort_by(|a, b| a.mount.cmp(&b.mount));
    Ok(result)
}

impl Volume {
    pub fn same(&self, other: &Self) -> bool {
        self.mount == other.mount && self.mount_id == other.mount_id
    }

    #[cfg(test)]
    pub(crate) fn test_volume(path: &Path) -> io::Result<Self> {
        Ok(Self {
            mount: path.to_owned(),
            label: String::new(),
            filesystem: "ext4".to_owned(),
            available: 0,
            mount_id: mount_id(&File::open(path)?)?,
        })
    }
}

pub struct Session {
    volume: Volume,
    root: File,
}

impl Session {
    pub fn open(volume: &Volume) -> io::Result<Self> {
        let root = File::from(openat(
            CWD,
            &volume.mount,
            directory_flags(),
            Mode::empty(),
        )?);
        if mount_id(&root)? != volume.mount_id {
            return Err(changed());
        }
        Ok(Self {
            volume: volume.clone(),
            root,
        })
    }

    pub fn guard(&self) -> io::Result<()> {
        let current = openat(CWD, &self.volume.mount, directory_flags(), Mode::empty())
            .map(File::from)
            .map_err(|_| changed())?;
        if mount_id(&current)? != self.volume.mount_id {
            return Err(changed());
        }
        Ok(())
    }

    pub fn available(&self) -> io::Result<u64> {
        let stat = fstatvfs(&self.root)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }

    pub fn folder(&self, relative: &Path) -> io::Result<Folder> {
        let _ = parts(relative)?;
        let file = if relative.as_os_str().is_empty() {
            self.root.try_clone()?
        } else {
            File::from(openat2(
                &self.root,
                relative,
                directory_flags(),
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
            )?)
        };
        let path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        Ok(Folder {
            path,
            _handles: vec![file],
        })
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn mount_id(file: &File) -> io::Result<u64> {
    fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mount_escapes_without_losing_non_utf8_bytes() {
        assert_eq!(
            unescape(br"/media/my\040disk\134\011\377"),
            OsString::from_vec(b"/media/my disk\\\t\xff".to_vec())
        );
    }

    #[test]
    fn directory_lookup_does_not_follow_links() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let volume = Volume::test_volume(root.path())?;
        fs::create_dir(root.path().join("ordinary"))?;
        std::os::unix::fs::symlink(root.path(), root.path().join("link"))?;
        let session = Session::open(&volume)?;
        assert!(session.folder(Path::new("ordinary")).is_ok());
        assert!(session.folder(Path::new("link")).is_err());
        assert!(session.folder(Path::new("../")).is_err());
        Ok(())
    }
}
