use super::{Folder, Maintenance, changed, parts};
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
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{
        CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CM_Request_Device_EjectW, CR_SUCCESS,
    },
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
        SystemInformation::GetSystemDirectoryW,
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

    pub fn maintain(&self, operation: Maintenance) -> io::Result<String> {
        let drive = self.mount.to_string_lossy();
        let letter = drive
            .as_bytes()
            .first()
            .copied()
            .filter(u8::is_ascii_uppercase)
            .ok_or_else(changed)?;
        let drive_name = format!("{}:", char::from(letter));
        let verify = || {
            if !is_usb(&drive_name) || !self.same(&inspect(&self.mount).map_err(|_| changed())?) {
                return Err(changed());
            }
            Ok(())
        };
        verify()?;
        let script = match operation {
            Maintenance::FormatExfat => {
                r#"
                $principal = [Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()
                if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
                    throw 'Reopen USB Copy as administrator to format a volume.'
                }
                $formatted = Format-Volume -InputObject $volume -FileSystem exFAT -Confirm:$false
                if ($formatted.FileSystem -ine 'exFAT') { throw 'exFAT format could not be verified.' }
                Write-Output 'exFAT'
            "#
            }
            Maintenance::Eject => {
                r#"
                $physical = @(Get-CimInstance -ClassName Win32_DiskDrive -Filter ("Index = " + [uint32]$disk.Number))
                if ($physical.Count -ne 1 -or !$physical[0].PNPDeviceID) { throw 'Cannot identify the USB device.' }
                Write-Output $physical[0].PNPDeviceID
            "#
            }
        };
        let output = powershell(letter, self.guid.as_os_str(), script)?;
        match operation {
            Maintenance::FormatExfat if output.eq_ignore_ascii_case("exfat") => {
                Ok("已格式化为 exFAT".to_owned())
            }
            Maintenance::FormatExfat => Err(io::Error::other("Windows 未确认 exFAT 格式化结果")),
            Maintenance::Eject => {
                let device = usb_devnode(&output)?;
                verify()?;
                let mut veto = 0;
                let mut owner = [0; 260];
                let status = unsafe {
                    CM_Request_Device_EjectW(
                        device,
                        &mut veto,
                        owner.as_mut_ptr(),
                        owner.len() as u32,
                        0,
                    )
                };
                if status != CR_SUCCESS {
                    return Err(io::Error::other(format!(
                        "Windows 拒绝安全弹出，请关闭使用该设备的程序。状态 0x{status:X}，原因 {veto} {}",
                        from_wide(&owner).to_string_lossy()
                    )));
                }
                Ok("U 盘已安全弹出，可以拔出".to_owned())
            }
        }
    }
}

fn powershell(letter: u8, guid: &OsStr, operation: &str) -> io::Result<String> {
    // Use the system copy rather than searching the current directory or PATH.
    let mut directory = [0; 32768];
    let length =
        unsafe { GetSystemDirectoryW(directory.as_mut_ptr(), directory.len() as u32) } as usize;
    if length == 0 || length >= directory.len() {
        return Err(io::Error::last_os_error());
    }
    let exe = PathBuf::from(OsString::from_wide(&directory[..length]))
        .join(r"WindowsPowerShell\v1.0\powershell.exe");
    let checks = r#"
        $ErrorActionPreference = 'Stop'
        $ProgressPreference = 'SilentlyContinue'
        [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
        $expected = $env:USB_COPY_VOLUME_GUID
        $partition = Get-Partition -DriveLetter ([char]$env:USB_COPY_DRIVE_LETTER)
        if ($partition.AccessPaths -inotcontains $expected) { throw 'The selected volume changed.' }
        $disk = Get-Disk -Number $partition.DiskNumber
        if ($disk.BusType -ine 'USB' -or $disk.IsSystem -or $disk.IsBoot) { throw 'This is not a non-system USB disk.' }
        $volume = Get-Volume -Path $expected
        if ($volume.Path -ine $expected) { throw 'The selected volume changed.' }
    "#;
    let output = Command::new(exe)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("{checks}\n{operation}"))
        .env("USB_COPY_DRIVE_LETTER", char::from(letter).to_string())
        .env("USB_COPY_VOLUME_GUID", guid)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "Windows 存储操作失败：{}",
            error.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn usb_devnode(id: &str) -> io::Result<u32> {
    // Resolve the first physical USB device above the storage LUN, never the USB hub.
    if id.is_empty() || id.contains(['\0', '\n', '\r']) {
        return Err(io::Error::other("Windows 未返回单一的 USB 存储设备"));
    }
    let missing = || io::Error::other("无法识别可安全弹出的 USB 设备");
    let mut node = 0;
    let id = wide(OsStr::new(id));
    if unsafe { CM_Locate_DevNodeW(&mut node, id.as_ptr(), 0) } != CR_SUCCESS {
        return Err(missing());
    }
    for _ in 0..12 {
        let mut buffer = [0; 260];
        if unsafe { CM_Get_Device_IDW(node, buffer.as_mut_ptr(), buffer.len() as u32, 0) }
            != CR_SUCCESS
        {
            break;
        }
        let identity = from_wide(&buffer).to_string_lossy().to_ascii_uppercase();
        if identity.starts_with(r"USB\VID_")
            && !identity
                .split('\\')
                .nth(1)
                .is_some_and(|kind| kind.contains("&MI_"))
        {
            return Ok(node);
        }
        let mut parent = 0;
        if unsafe { CM_Get_Parent(&mut parent, node, 0) } != CR_SUCCESS {
            break;
        }
        node = parent;
    }
    Err(missing())
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
