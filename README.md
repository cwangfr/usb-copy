# U 盘复制工具

<img src="assets/usb-copy.svg" width="80" height="80" alt="U 盘复制工具图标">

Rust + egui 桌面应用，为已挂载的 USB 存储卷提供文件和目录复制、永久删除与 SHA-256 校验。

## 运行

从 [GitHub Releases](https://github.com/cwangfr/usb-copy/releases/latest) 下载 Linux 或 Windows 可执行文件，或选择 Debian / Ubuntu 的 amd64 安装包。下载 `.deb` 后，在文件所在目录运行 `sudo apt install ./usb-copy_0.2.0_amd64.deb`。

- Linux x86-64 使用 `dist/usb-copy-linux-x86_64`。如果下载后执行权限丢失，先运行 `chmod +x dist/usb-copy-linux-x86_64`。需要 Linux 内核 5.6 或更新版本、glibc 2.35 或更新版本，以及可用的 X11 或 Wayland 和 OpenGL 环境。
- Windows x64 双击 `dist/usb-copy-windows-x86_64.exe`。USB 卷需要分配盘符。
- 每个平台只分发一个可执行文件，中文字体已内嵌。Linux 原生文件对话框依赖桌面文件选择服务，也可以使用「输入路径」或拖放。

## 安装到 Linux 应用菜单

从源码运行 `cargo build --release --locked` 后，在项目目录执行以下命令。使用已构建的 Linux 发布文件时，可将第一条命令的源路径换成 `dist/usb-copy-linux-x86_64`。

```sh
install -Dm755 target/release/usb-copy "$HOME/.local/bin/usb-copy"
install -Dm644 assets/usb-copy.svg "$HOME/.local/share/icons/hicolor/scalable/apps/usb-copy.svg"
mkdir -p "$HOME/.local/share/applications"
sed "s|^Exec=.*|Exec=\"$HOME/.local/bin/usb-copy\"|" packaging/usb-copy.desktop \
  > "$HOME/.local/share/applications/usb-copy.desktop"
```

## 使用

1. 插入并挂载 U 盘。点击「刷新」并选择目标卷。
2. 添加源文件或文件夹，在右侧进入目标目录，点击「开始复制到此处」。
3. 「高级设置」中的读写块默认为 1 MiB，定期文件同步间隔默认为 64 MiB。设置为 0 时只在每个文件写完后同步。默认跳过同名顶层项目，也可以启用自动改名。
4. 通过「任务结果」查看成功、跳过和失败的项目。可导出 `.sha256` 清单；导入时，在右侧进入清单对应的基准目录。也可选中 U 盘中的文件直接计算哈希或比对预期值。
5. 删除前核对确认框中的卷、目录和所选内容。删除为永久操作，链接、目录联接和特殊文件会被拒绝。取消任务会保留此前已完成的文件和目录。
6. 用「格式化…」将所选 USB 卷快速格式化为 exFAT。输入 `FORMAT` 后才能执行，操作会清除该卷的数据。exFAT 支持超过 4 GiB 的单个文件。
7. 拔出设备前点击「安全弹出…」，等待系统确认。如果系统报告设备忙，请关闭使用它的程序再试。

格式化和弹出仅适用于程序识别到的已挂载 USB 存储。Linux 使用 UDisks2 和 `busctl`，格式化还需要 `exfatprogs`。Linux 设备操作需要内核提供磁盘序号（sysfs 中的 `diskseq`）。Windows 格式化需要以管理员身份启动程序，使用系统的 `Format-Volume`。弹出使用 Windows 即插即用接口。

复制先写同一目录中的临时文件。同步成功且回读的 SHA-256 一致后，才以禁止覆盖的方式提交正式文件名。拔盘中断后，如果留有临时文件，在其所在目录会显示「未完成」。普通回读可以命中系统缓存，重要数据可在系统安全弹出、重新插入后，使用保存的清单再次校验。

## 构建与测试

在目标平台上使用 Rust 1.92 或更新版本。

```text
cargo build --release --locked
cargo test --locked
```

`src/copy.rs` 实现分块写入、同步和回读校验。`src/jobs.rs` 实现目录任务和校验清单。`src/volume/` 处理平台卷识别和目录访问。`src/ui.rs` 实现界面。
