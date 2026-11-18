use super::{Folder, Maintenance, changed, parts};
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
    process::Command,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub mount: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub available: u64,
    mount_id: u64,
    device: PathBuf,
    disk_sequence: Option<u64>,
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
            disk_sequence: disk_sequence(disk(&device)).ok(),
            device,
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
            device: PathBuf::new(),
            disk_sequence: None,
        })
    }
}

fn disk(device: &Path) -> &Path {
    if device.join("partition").is_file() {
        device.parent().unwrap_or(device)
    } else {
        device
    }
}

fn disk_sequence(device: &Path) -> io::Result<u64> {
    fs::read_to_string(device.join("diskseq"))?
        .trim()
        .parse()
        .map_err(io::Error::other)
}

fn block_object(device: &Path) -> io::Result<String> {
    let name = device
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(io::Error::other("无法识别 UDisks 的 USB 块设备"));
    }
    Ok(format!("/org/freedesktop/UDisks2/block_devices/{name}"))
}

const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const MANAGER: &str = "org.freedesktop.UDisks2.Manager";

fn udisks(args: &[&str]) -> io::Result<String> {
    let (command, rest) = args.split_first().unwrap();
    let result = Command::new("/usr/bin/busctl")
        .args([
            "--system",
            "--allow-interactive-authorization=yes",
            "--timeout=600",
            command,
            "org.freedesktop.UDisks2",
        ])
        .args(rest)
        .output()
        .map_err(|error| io::Error::other(format!("需要可用的 UDisks2 和 busctl：{error}")))?;
    if !result.status.success() {
        let detail = String::from_utf8_lossy(&result.stderr);
        return Err(io::Error::other(format!("UDisks2：{}", detail.trim())));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn usb_parent(device: &Path) -> Option<&Path> {
    device.ancestors().find(|ancestor| {
        fs::read_link(ancestor.join("subsystem")).is_ok_and(|value| value.ends_with("usb"))
            && ancestor.join("idVendor").is_file()
    })
}

fn backs_system(device: &Path, target: &Path) -> bool {
    if disk(device) == target {
        return true;
    }
    fs::read_dir(device.join("slaves"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::canonicalize(entry.path()).ok())
        .any(|slave| backs_system(&slave, target))
}

impl Volume {
    pub fn maintain(&self, operation: Maintenance) -> io::Result<String> {
        let protected = || io::Error::other("无法安全确认此 USB 设备；请使用系统磁盘工具");
        let physical = disk(&self.device);
        let Some(sequence) = self.disk_sequence else {
            return Err(protected());
        };
        let device_current = || {
            if disk_sequence(physical).ok() != Some(sequence) {
                return Err(changed());
            }
            let number = fs::read_to_string(self.device.join("dev")).map_err(|_| changed())?;
            if usb_device(number.trim()).as_deref() != Some(self.device.as_path()) {
                return Err(changed());
            }
            Ok(())
        };
        device_current()?;
        Session::open(self)?.guard()?;
        let block = block_object(&self.device)?;
        let disk_block = block_object(physical)?;
        let mounted = mounts()?;
        for mount in &mounted {
            if mount.id != self.mount_id
                && mount_device(mount).is_some_and(|device| {
                    device == self.device
                        || (operation == Maintenance::Eject && disk(&device) == physical)
                })
            {
                return Err(io::Error::other("设备仍有其他挂载点，请先卸载这些挂载点"));
            }
            if ["/", "/boot", "/boot/efi", "/home", "/usr", "/var"]
                .iter()
                .any(|path| mount.path == Path::new(path))
                && fs::canonicalize(Path::new("/sys/dev/block").join(&mount.device))
                    .is_ok_and(|device| backs_system(&device, physical))
            {
                return Err(protected());
            }
        }
        if udisks(&["get-property", &block, BLOCK, "HintSystem"])? != "b false"
            || udisks(&["get-property", &disk_block, BLOCK, "HintSystem"])? != "b false"
        {
            return Err(protected());
        }
        let drive = udisks(&["get-property", &block, BLOCK, "Drive"])?;
        let drive = drive
            .strip_prefix("o \"/org/freedesktop/UDisks2/drives/")
            .and_then(|_| drive.strip_prefix("o \"")?.strip_suffix('"'))
            .ok_or_else(protected)?;
        if udisks(&["get-property", drive, DRIVE, "ConnectionBus"])? != "s \"usb\"" {
            return Err(protected());
        }

        match operation {
            Maintenance::FormatExfat => {
                if udisks(&["get-property", &block, BLOCK, "ReadOnly"])? != "b false"
                    || udisks(&["get-property", &block, BLOCK, "IdUsage"])? != "s \"filesystem\""
                {
                    return Err(protected());
                }
                if self.device == physical
                    && fs::read_dir(physical)?
                        .flatten()
                        .any(|child| child.path().join("partition").is_file())
                {
                    return Err(protected());
                }
                let supported = udisks(&[
                    "call",
                    "/org/freedesktop/UDisks2/Manager",
                    MANAGER,
                    "CanFormat",
                    "s",
                    "exfat",
                ])?;
                if !supported.starts_with("(bs) true ") && !supported.starts_with("bs true ") {
                    return Err(io::Error::other(
                        "UDisks2 无法格式化 exFAT，请检查是否安装了 exfatprogs",
                    ));
                }
            }
            Maintenance::Eject => {
                let parent = usb_parent(physical).ok_or_else(protected)?;
                let siblings = fs::read_dir("/sys/class/block")?
                    .filter_map(|entry| fs::canonicalize(entry.ok()?.path()).ok())
                    .filter(|entry| {
                        !entry.join("partition").is_file() && usb_parent(entry) == Some(parent)
                    })
                    .count();
                if siblings != 1
                    || udisks(&["get-property", drive, DRIVE, "CanPowerOff"])? != "b true"
                {
                    return Err(io::Error::other(
                        "此 USB 设备无法独立断电，请使用系统安全弹出",
                    ));
                }
            }
        }
        device_current()?;
        Session::open(self)?.guard()?;
        udisks(&["call", &block, FILESYSTEM, "Unmount", "a{sv}", "0"])?;
        device_current()?;
        match operation {
            Maintenance::FormatExfat => {
                let formatted = if self.device == physical {
                    udisks(&["call", &block, BLOCK, "Format", "sa{sv}", "exfat", "0"])
                } else {
                    udisks(&[
                        "call",
                        &block,
                        BLOCK,
                        "Format",
                        "sa{sv}",
                        "exfat",
                        "1",
                        "update-partition-type",
                        "b",
                        "true",
                    ])
                };
                formatted
                    .map_err(|error| io::Error::other(format!("卷已卸载，格式化失败。{error}")))?;
                device_current()?;
                match udisks(&["call", &block, FILESYSTEM, "Mount", "a{sv}", "0"]) {
                    Ok(_) => Ok("已格式化为 exFAT 并重新挂载".to_owned()),
                    Err(_)
                        if mounts().is_ok_and(|mounts| {
                            mounts.iter().any(|mount| {
                                mount_device(mount).as_deref() == Some(self.device.as_path())
                            })
                        }) =>
                    {
                        Ok("已格式化为 exFAT 并重新挂载".to_owned())
                    }
                    Err(error) => Ok(format!("已格式化为 exFAT，请手动重新挂载。{error}")),
                }
            }
            Maintenance::Eject => {
                udisks(&["call", drive, DRIVE, "PowerOff", "a{sv}", "0"]).map_err(|error| {
                    io::Error::other(format!(
                        "卷已卸载，但安全弹出失败。请通过系统安全弹出。{error}"
                    ))
                })?;
                Ok("U 盘已安全弹出，可以拔出".to_owned())
            }
        }
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

    #[test]
    fn device_operations_reject_an_ordinary_directory() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("keep");
        fs::write(&file, b"keep")?;
        let volume = Volume::test_volume(root.path())?;
        assert!(volume.maintain(Maintenance::FormatExfat).is_err());
        assert!(volume.maintain(Maintenance::Eject).is_err());
        assert_eq!(fs::read(file)?, b"keep");
        Ok(())
    }
}
