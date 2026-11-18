use std::{
    ffi::OsStr,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};

#[cfg(target_os = "linux")]
#[path = "volume/linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "volume/windows.rs"]
mod platform;

pub use platform::{Session, Volume, detect};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Maintenance {
    FormatExfat,
    Eject,
}

impl Maintenance {
    pub fn progress(self) -> &'static str {
        match self {
            Self::FormatExfat => "正在格式化为 exFAT",
            Self::Eject => "正在安全弹出",
        }
    }
}

pub struct Folder {
    path: PathBuf,
    _handles: Vec<File>,
}

impl Folder {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn parts(relative: &Path) -> io::Result<impl Iterator<Item = &OsStr>> {
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "路径超出所选目录",
        ));
    }
    Ok(relative.components().map(|component| component.as_os_str()))
}

pub fn is_fat32(filesystem: &str) -> bool {
    filesystem.eq_ignore_ascii_case("fat32") || filesystem.eq_ignore_ascii_case("vfat")
}

pub fn checked_entry(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::copy::is_link(&metadata) || !(metadata.is_file() || metadata.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "不支持链接、目录联接或特殊文件",
        ));
    }
    Ok(metadata)
}

fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "U 盘已移除或目标卷已改变，请重新选择",
    )
}
