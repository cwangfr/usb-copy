use super::{Folder, changed, parts};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io,
    mem::offset_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        BusTypeUsb, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    },
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
            STORAGE_PROPERTY_QUERY, StorageDeviceProperty, VOLUME_DISK_EXTENTS,
        },
        WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub mount: PathBuf,
    pub label: String,
    pub filesystem: String,
    pub available: u64,
    guid: PathBuf,
    serial: u32,
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn from_wide(buffer: &[u16]) -> OsString {
    OsString::from_wide(
        &buffer[..buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len())],
    )
}

pub fn detect() -> io::Result<Vec<Volume>> {
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return Err(io::Error::last_os_error());
    }
    let system = env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
    let mut result = Vec::new();
    for index in 0..26 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index) as char;
        let drive = format!("{letter}:");
        if drive.eq_ignore_ascii_case(&system) || !is_usb(&drive) {
            continue;
        }
        if let Ok(volume) = inspect(Path::new(&format!("{drive}\\"))) {
            result.push(volume);
        }
    }
    Ok(result)
}

fn is_usb(drive: &str) -> bool {
    let root = wide(OsStr::new(&format!("{drive}\\")));
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    if kind != DRIVE_FIXED && kind != DRIVE_REMOVABLE {
        return false;
    }
    let Ok(device) = open_device(&format!(r"\\.\{drive}")) else {
        return false;
    };
    if usb_bus(&device) {
        return true;
    }
    // Some volume drivers only expose the bus property on the backing disk.
    let mut extents = VOLUME_DISK_EXTENTS::default();
    let mut written = 0;
    let status = unsafe {
        DeviceIoControl(
            device.as_raw_handle(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            null_mut(),
            0,
            (&raw mut extents).cast(),
            size_of::<VOLUME_DISK_EXTENTS>() as u32,
            &mut written,
            null_mut(),
        )
    };
    status != 0
        && written as usize >= size_of::<VOLUME_DISK_EXTENTS>()
        && extents.NumberOfDiskExtents == 1
        && open_device(&format!(
            r"\\.\PhysicalDrive{}",
            extents.Extents[0].DiskNumber
        ))
        .is_ok_and(|disk| usb_bus(&disk))
}

fn open_device(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

fn usb_bus(device: &File) -> bool {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut descriptor = [0u8; 4096];
    let mut written = 0;
    let status = unsafe {
        DeviceIoControl(
            device.as_raw_handle(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            descriptor.as_mut_ptr().cast(),
            descriptor.len() as u32,
            &mut written,
            null_mut(),
        )
    };
    let field = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
    let end = field + size_of::<i32>();
    status != 0 && written as usize >= end && descriptor[field..end] == BusTypeUsb.to_ne_bytes()
}

fn inspect(mount: &Path) -> io::Result<Volume> {
    let mount_wide = wide(mount.as_os_str());
    let mut guid = [0; 64];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(mount_wide.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut label = [0; 256];
    let mut filesystem = [0; 32];
    let mut serial = 0;
    if unsafe {
        GetVolumeInformationW(
            guid.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            &mut serial,
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let label = from_wide(&label).to_string_lossy().into_owned();
    let guid = PathBuf::from(from_wide(&guid));
    Ok(Volume {
        mount: mount.to_owned(),
        label: if label.is_empty() {
            mount.display().to_string()
        } else {
            label
        },
        filesystem: from_wide(&filesystem).to_string_lossy().into_owned(),
        available: available(&guid)?,
        guid,
        serial,
    })
}

fn available(path: &Path) -> io::Result<u64> {
    let path_wide = wide(path.as_os_str());
    let mut bytes = 0;
    if unsafe { GetDiskFreeSpaceExW(path_wide.as_ptr(), &mut bytes, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

impl Volume {
    pub fn same(&self, other: &Self) -> bool {
        self.mount == other.mount && self.guid == other.guid && self.serial == other.serial
    }
}

pub struct Session {
    volume: Volume,
    root: File,
}

impl Session {
    pub fn open(volume: &Volume) -> io::Result<Self> {
        let root = open_directory(&volume.guid)?;
        let session = Self {
            volume: volume.clone(),
            root,
        };
        session.guard()?;
        Ok(session)
    }

    pub fn guard(&self) -> io::Result<()> {
        let current = inspect(&self.volume.mount).map_err(|_| changed())?;
        if !self.volume.same(&current) {
            return Err(changed());
        }
        Ok(())
    }

    pub fn available(&self) -> io::Result<u64> {
        available(&self.volume.guid)
    }

    pub fn folder(&self, relative: &Path) -> io::Result<Folder> {
        let mut path = self.volume.guid.clone();
        let mut handles = vec![self.root.try_clone()?];
        for part in parts(relative)? {
            // A colon would address an alternate data stream on a Windows path.
            if part.encode_wide().any(|unit| unit == u16::from(b':')) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "无效的路径成分",
                ));
            }
            path.push(part);
            handles.push(open_directory(&path)?);
        }
        Ok(Folder {
            path,
            _handles: handles,
        })
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    // Keeping delete sharing disabled pins directories while they are used as parents.
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || crate::copy::is_link(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "路径不是普通目录",
        ));
    }
    Ok(file)
}
