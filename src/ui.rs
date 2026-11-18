use eframe::egui::{self, Color32, RichText};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::Duration,
};
use usb_copy::{
    copy::Settings,
    jobs::{self, Job, Monitor, Outcome, Report},
    volume::{self, Maintenance, Session, Volume},
};

struct Entry {
    name: OsString,
    directory: bool,
    link: bool,
    bytes: u64,
}

struct Listing {
    volumes: Vec<Volume>,
    selected: Option<usize>,
    folder: PathBuf,
    entries: Vec<Entry>,
    error: Option<String>,
}

struct ActiveJob {
    monitor: Arc<Monitor>,
    receive: Receiver<Report>,
    action: &'static str,
    base: PathBuf,
    device_operation: bool,
}

#[derive(Clone, Copy)]
enum PathAction {
    AddSource,
    ImportManifest,
    ExportManifest,
}

pub struct App {
    sources: Vec<PathBuf>,
    source_selection: BTreeSet<usize>,
    listing: Listing,
    selection: BTreeSet<usize>,
    browse: Option<Receiver<io::Result<Listing>>>,
    active: Option<ActiveJob>,
    report: Option<Report>,
    report_base: PathBuf,
    show_report: bool,
    show_settings: bool,
    show_license: bool,
    confirm_delete: bool,
    confirm_device: Option<(Maintenance, Volume)>,
    format_confirmation: String,
    show_hash: bool,
    expected: String,
    path_input: Option<(PathAction, String)>,
    settings: Settings,
    rename: bool,
    status: String,
    issue: bool,
    exit_after_task: bool,
}

impl App {
    pub fn new(creation: &eframe::CreationContext<'_>) -> Self {
        let context = &creation.egui_ctx;
        let mut fonts = egui::FontDefinitions::default();
        let family = "Droid Sans Fallback".to_owned();
        fonts.font_data.insert(
            family.clone(),
            Arc::new(egui::FontData::from_static(include_bytes!(
                "../assets/DroidSansFallbackFull.ttf"
            ))),
        );
        for font_chain in fonts.families.values_mut() {
            font_chain.push(family.clone());
        }
        context.set_fonts(fonts);
        context.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(9.0, 8.0);
            style.spacing.button_padding = egui::vec2(10.0, 5.0);
        });
        let mut app = Self {
            sources: Vec::new(),
            source_selection: BTreeSet::new(),
            listing: Listing {
                volumes: Vec::new(),
                selected: None,
                folder: PathBuf::new(),
                entries: Vec::new(),
                error: None,
            },
            selection: BTreeSet::new(),
            browse: None,
            active: None,
            report: None,
            report_base: PathBuf::new(),
            show_report: false,
            show_settings: false,
            show_license: false,
            confirm_delete: false,
            confirm_device: None,
            format_confirmation: String::new(),
            show_hash: false,
            expected: String::new(),
            path_input: None,
            settings: Settings::default(),
            rename: false,
            status: String::new(),
            issue: false,
            exit_after_task: false,
        };
        app.refresh(context);
        app
    }

    fn current(&self) -> Option<&Volume> {
        let index = self.listing.selected?;
        self.listing.volumes.get(index)
    }

    fn refresh(&mut self, context: &egui::Context) {
        let previous = self.current().cloned();
        let previous_folder = self.listing.folder.clone();
        self.list(context, move || {
            let volumes = volume::detect()?;
            let selected = previous
                .as_ref()
                .and_then(|old| volumes.iter().position(|item| item.same(old)));
            let selected =
                selected.or_else(|| (previous.is_none() && volumes.len() == 1).then_some(0));
            let folder = if previous.is_some() && selected.is_some() {
                previous_folder
            } else {
                PathBuf::new()
            };
            Ok(load_listing(volumes, selected, folder))
        });
    }

    fn navigate(&mut self, context: &egui::Context, selected: Option<usize>, folder: PathBuf) {
        let volumes = self.listing.volumes.clone();
        self.list(context, move || Ok(load_listing(volumes, selected, folder)));
    }

    fn list(
        &mut self,
        context: &egui::Context,
        read: impl FnOnce() -> io::Result<Listing> + Send + 'static,
    ) {
        self.selection.clear();
        let (send, receive) = mpsc::channel();
        let context = context.clone();
        thread::spawn(move || {
            let _ = send.send(read());
            context.request_repaint();
        });
        self.browse = Some(receive);
    }

    fn start(&mut self, context: &egui::Context, action: &'static str, job: Job) {
        let Some(volume) = self.current().cloned() else {
            return;
        };
        let device_operation = matches!(job, Job::Device(_));
        let job_base = if device_operation {
            PathBuf::new()
        } else {
            self.listing.folder.clone()
        };
        let base = volume.mount.join(&job_base);
        let monitor = Arc::new(Monitor::default());
        let worker_monitor = monitor.clone();
        let (send, receive) = mpsc::channel();
        let context = context.clone();
        thread::spawn(move || {
            let _ = send.send(jobs::run(&volume, &job_base, job, &worker_monitor));
            context.request_repaint();
        });
        self.active = Some(ActiveJob {
            monitor,
            receive,
            action,
            base,
            device_operation,
        });
        self.status.clear();
        self.issue = false;
        self.show_report = false;
    }

    fn poll(&mut self, context: &egui::Context) {
        if let Some(receiver) = &self.browse {
            match receiver.try_recv() {
                Ok(Ok(listing)) => {
                    self.listing = listing;
                    self.browse = None;
                }
                Ok(Err(error)) => {
                    self.listing = Listing {
                        volumes: Vec::new(),
                        selected: None,
                        folder: PathBuf::new(),
                        entries: Vec::new(),
                        error: Some(error.to_string()),
                    };
                    self.browse = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.listing.entries.clear();
                    self.listing.error = Some("无法读取 U 盘列表".to_owned());
                    self.browse = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some(active) = &self.active else { return };
        if context.input(|input| input.viewport().close_requested()) {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.exit_after_task = true;
            if !active.device_operation {
                active.monitor.cancel.store(true, Ordering::Relaxed);
            }
        }
        match active.receive.try_recv() {
            Ok(report) => {
                let mut counts = [0; 3];
                for row in &report.rows {
                    counts[match row.outcome {
                        Outcome::Passed => 0,
                        Outcome::Skipped => 1,
                        Outcome::Failed => 2,
                    }] += 1;
                }
                let [passed, skipped, failed] = counts;
                let state = if report.cancelled {
                    "已取消"
                } else {
                    "结束"
                };
                self.status = format!(
                    "{}{state} · 成功 {passed} · 跳过 {skipped} · 失败 {failed}",
                    active.action
                );
                if active.device_operation
                    && let Some(row) = report.rows.first()
                {
                    self.status = row.detail.clone();
                }
                if !report.cancelled
                    && (active.action == "复制" || active.action == "删除")
                    && passed > 0
                {
                    self.status.push_str("。拔盘前请选择「安全弹出」");
                }
                self.issue = failed > 0;
                self.report_base = active.base.clone();
                self.report = Some(report);
                self.active = None;
                self.refresh(context);
                if self.exit_after_task {
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.active = None;
                self.issue = true;
                self.status = "后台任务意外结束，请检查目标文件".to_owned();
            }
            Err(TryRecvError::Empty) => context.request_repaint_after(Duration::from_millis(120)),
        }
    }

    fn add_sources(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if !self.sources.contains(&path) {
                self.sources.push(path);
            }
        }
    }

    fn pick(&mut self, directory: bool) {
        let dialog = rfd::FileDialog::new();
        let paths = if directory {
            dialog.pick_folders()
        } else {
            dialog.pick_files()
        };
        if let Some(paths) = paths {
            self.add_sources(paths);
        }
    }

    fn names(&self) -> Vec<PathBuf> {
        self.selection
            .iter()
            .filter_map(|&index| self.listing.entries.get(index))
            .map(|entry| PathBuf::from(&entry.name))
            .collect()
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("U 盘复制工具").size(25.0).strong());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("目标 U 盘").strong());
            let previous = self.listing.selected;
            let mut selected = previous;
            let label = self
                .current()
                .map(|volume| format!("{}  ·  {}", volume.label, volume.mount.display()))
                .unwrap_or_else(|| "选择目标卷".to_owned());
            ui.add_enabled_ui(self.active.is_none() && self.browse.is_none(), |ui| {
                egui::ComboBox::from_id_salt("target_volume")
                    .width(260.0)
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (index, volume) in self.listing.volumes.iter().enumerate() {
                            ui.selectable_value(
                                &mut selected,
                                Some(index),
                                format!("{}  ·  {}", volume.label, volume.mount.display()),
                            );
                        }
                        if self.listing.volumes.is_empty() {
                            ui.weak("没有检测到已挂载的 U 盘");
                        }
                    });
                if ui.button("刷新").clicked() {
                    self.refresh(ui.ctx());
                }
            });
            if selected != previous {
                self.navigate(ui.ctx(), selected, PathBuf::new());
            }
            if self.browse.is_some() {
                ui.spinner();
            } else if let Some(volume) = self.current() {
                ui.weak(format!(
                    "{}  ·  可用 {}",
                    volume.filesystem.to_uppercase(),
                    size(volume.available)
                ));
            }
        });
        ui.horizontal(|ui| {
            let ready = self.active.is_none() && self.browse.is_none() && self.current().is_some();
            for (operation, label) in [
                (Maintenance::Eject, "安全弹出…"),
                (Maintenance::FormatExfat, "格式化…"),
            ] {
                if ui.add_enabled(ready, egui::Button::new(label)).clicked()
                    && let Some(volume) = self.current().cloned()
                {
                    self.format_confirmation.clear();
                    self.confirm_device = Some((operation, volume));
                }
            }
        });
    }

    fn sources(&mut self, ui: &mut egui::Ui, height: f32) {
        ui.heading("待复制");
        ui.horizontal_wrapped(|ui| {
            if ui.button("添加文件").clicked() {
                self.pick(false);
            }
            if ui.button("添加文件夹").clicked() {
                self.pick(true);
            }
            if ui.button("输入路径").clicked() {
                self.path_input = Some((PathAction::AddSource, String::new()));
            }
            if ui
                .add_enabled(
                    !self.source_selection.is_empty(),
                    egui::Button::new("移除所选"),
                )
                .clicked()
            {
                for &index in self.source_selection.iter().rev() {
                    self.sources.remove(index);
                }
                self.source_selection.clear();
            }
        });
        if self.sources.is_empty() {
            ui.weak("选择或拖入文件和文件夹。");
        }
        egui::ScrollArea::vertical()
            .id_salt("sources")
            .max_height(height)
            .show_rows(ui, 23.0, self.sources.len(), |ui, rows| {
                for index in rows {
                    let path = &self.sources[index];
                    let mut selected = self.source_selection.contains(&index);
                    let title = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy();
                    if ui
                        .checkbox(&mut selected, title)
                        .on_hover_text(path.display().to_string())
                        .changed()
                    {
                        if selected {
                            self.source_selection.insert(index);
                        } else {
                            self.source_selection.remove(&index);
                        }
                    }
                }
            });
    }

    fn files(&mut self, ui: &mut egui::Ui, height: f32, ready: bool) {
        ui.heading("U 盘内容");
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    ready && !self.listing.folder.as_os_str().is_empty(),
                    egui::Button::new("上一级"),
                )
                .clicked()
            {
                self.navigate(
                    ui.ctx(),
                    self.listing.selected,
                    self.listing.folder.parent().unwrap().to_owned(),
                );
            }
            let label = self.current().map(|volume| {
                volume
                    .mount
                    .join(&self.listing.folder)
                    .display()
                    .to_string()
            });
            ui.weak(label.unwrap_or_else(|| "请先挂载并选择 U 盘".to_owned()));
        });
        ui.horizontal_wrapped(|ui| {
            let hashable = !self.selection.is_empty()
                && self.selection.iter().all(|&index| {
                    let entry = &self.listing.entries[index];
                    !entry.directory && !entry.link
                });
            if ui
                .add_enabled(ready && hashable, egui::Button::new("SHA-256"))
                .clicked()
            {
                self.expected.clear();
                self.show_hash = true;
            }
            if ui
                .add_enabled(ready, egui::Button::new("导入校验清单"))
                .clicked()
            {
                self.path_input = Some((PathAction::ImportManifest, String::new()));
            }
            if ui
                .add_enabled(
                    ready && !self.selection.is_empty(),
                    egui::Button::new("删除…"),
                )
                .clicked()
            {
                self.confirm_delete = true;
            }
        });
        if let Some(error) = &self.listing.error {
            ui.colored_label(error_color(ui), error);
        } else if self.current().is_some()
            && self.listing.entries.is_empty()
            && self.browse.is_none()
        {
            ui.weak("此文件夹为空。");
        }

        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("files")
            .max_height((height - 36.0).max(80.0))
            .show_rows(ui, 23.0, self.listing.entries.len(), |ui, rows| {
                for index in rows {
                    let entry = &self.listing.entries[index];
                    ui.horizontal(|ui| {
                        let mut selected = self.selection.contains(&index);
                        if ui
                            .add_enabled(
                                ready && !entry.link,
                                egui::Checkbox::without_text(&mut selected),
                            )
                            .changed()
                        {
                            if selected {
                                self.selection.insert(index);
                            } else {
                                self.selection.remove(&index);
                            }
                        }
                        let name = entry.name.to_string_lossy();
                        if entry.directory && !entry.link {
                            if ui
                                .add_enabled(
                                    ready,
                                    egui::Button::new(format!("{name}/")).frame(false),
                                )
                                .clicked()
                            {
                                open = Some(self.listing.folder.join(&entry.name));
                            }
                        } else {
                            ui.add(egui::Label::new(name.as_ref()).truncate())
                                .on_hover_text(name.as_ref());
                            if entry.link {
                                ui.weak("链接");
                            } else if jobs::temp_name(&entry.name) {
                                ui.colored_label(error_color(ui), "未完成");
                            }
                            if !entry.link {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| ui.weak(size(entry.bytes)),
                                );
                            }
                        }
                    });
                }
            });
        if let Some(folder) = open {
            self.navigate(ui.ctx(), self.listing.selected, folder);
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui, ready: bool) {
        if let Some(active) = &self.active {
            let progress = active.monitor.progress.lock().unwrap().clone();
            let cancelled = active.monitor.cancel.load(Ordering::Relaxed);
            ui.horizontal(|ui| {
                ui.spinner();
                let label = if self.exit_after_task && active.device_operation {
                    "操作结束后将关闭窗口"
                } else if cancelled {
                    "正在取消，等待当前操作结束"
                } else {
                    progress.stage
                };
                ui.strong(label);
                if progress.total > 0 {
                    ui.weak(format!("{} / {} 项", progress.finished, progress.total));
                }
            });
            if !progress.item.as_os_str().is_empty() {
                ui.add(egui::Label::new(progress.item.display().to_string()).truncate());
            }
            if progress.size > 0 {
                let fraction = progress.bytes as f64 / progress.size as f64;
                ui.add(
                    egui::ProgressBar::new(fraction.clamp(0.0, 1.0) as f32).text(format!(
                        "{} / {}",
                        size(progress.bytes),
                        size(progress.size)
                    )),
                );
            }
        } else if !self.status.is_empty() {
            if self.issue {
                ui.add(
                    egui::Label::new(RichText::new(&self.status).color(error_color(ui))).truncate(),
                )
                .on_hover_text(&self.status);
            } else {
                ui.label(&self.status);
            }
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.active.is_none(), egui::Button::new("高级设置"))
                .clicked()
            {
                self.show_settings = true;
            }
            if ui
                .add_enabled(
                    self.active.is_none() && self.report.is_some(),
                    egui::Button::new("任务结果"),
                )
                .clicked()
            {
                self.show_report = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(active) = &self.active {
                    if !active.device_operation
                        && ui
                            .add_enabled(
                                !active.monitor.cancel.load(Ordering::Relaxed),
                                egui::Button::new("取消任务"),
                            )
                            .clicked()
                    {
                        active.monitor.cancel.store(true, Ordering::Relaxed);
                    }
                } else {
                    let button = egui::Button::new(RichText::new("开始复制到此处").strong())
                        .selected(ready && !self.sources.is_empty());
                    if ui
                        .add_enabled(ready && !self.sources.is_empty(), button)
                        .clicked()
                    {
                        self.start(
                            ui.ctx(),
                            "复制",
                            Job::Copy {
                                sources: self.sources.clone(),
                                settings: self.settings,
                                rename: self.rename,
                            },
                        );
                    }
                }
            });
        });
    }

    fn dialogs(&mut self, context: &egui::Context) {
        self.device_dialog(context);
        if self.confirm_delete {
            let names = self.names();
            let response = egui::Modal::new(egui::Id::new("confirm_delete")).show(context, |ui| {
                ui.heading("永久删除 U 盘内容？");
                if let Some(volume) = self.current() {
                    ui.label(format!(
                        "{}  ·  {}",
                        volume.label,
                        volume.mount.join(&self.listing.folder).display()
                    ));
                }
                ui.label(format!(
                    "将删除 {} 个所选项目。文件夹中的内容也会被删除。",
                    names.len()
                ));
                egui::ScrollArea::vertical().max_height(200.0).show_rows(
                    ui,
                    20.0,
                    names.len(),
                    |ui, rows| {
                        for index in rows {
                            ui.label(names[index].display().to_string());
                        }
                    },
                );
                ui.horizontal(|ui| {
                    let cancel = ui.button("取消").clicked();
                    let delete = ui
                        .button(RichText::new("永久删除").color(error_color(ui)))
                        .clicked();
                    (cancel, delete)
                })
                .inner
            });
            let (cancel, delete) = response.inner;
            if delete {
                self.start(context, "删除", Job::Delete(names));
            }
            if delete || cancel || response.should_close() {
                self.confirm_delete = false;
            }
        }

        if self.show_hash {
            let names = self.names();
            let response = egui::Modal::new(egui::Id::new("hash")).show(context, |ui| {
                ui.heading("SHA-256 校验");
                ui.label(format!("已选择 {} 个文件", names.len()));
                if names.len() == 1 {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.expected)
                            .desired_width(360.0)
                            .hint_text("粘贴预期 SHA-256（可留空）"),
                    );
                }
                let expected = jobs::parse_hash(&self.expected);
                let valid = self.expected.trim().is_empty() || expected.is_some();
                if !valid {
                    ui.colored_label(error_color(ui), "请输入 64 个十六进制字符。");
                }
                ui.horizontal(|ui| {
                    let cancel = ui.button("取消").clicked();
                    let start = ui
                        .add_enabled(valid && !names.is_empty(), egui::Button::new("开始校验"))
                        .clicked();
                    (cancel, start, expected)
                })
                .inner
            });
            let (cancel, start, expected) = response.inner;
            if start {
                self.start(context, "SHA-256 校验", Job::Hash { names, expected });
            }
            if start || cancel || response.should_close() {
                self.show_hash = false;
            }
        }

        if self.show_settings {
            egui::Window::new("高级设置")
                .open(&mut self.show_settings)
                .collapsible(false)
                .show(context, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("应用读写块");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.block_mib)
                                .range(1..=16)
                                .suffix(" MiB"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("定期同步间隔");
                        ui.add(
                            egui::DragValue::new(&mut self.settings.sync_mib)
                                .range(0..=4096)
                                .suffix(" MiB"),
                        );
                    });
                    ui.weak("0 表示仅在每个文件写入结束时同步。");
                    ui.separator();
                    ui.checkbox(&mut self.rename, "顶层项目同名时自动改名");
                    ui.weak("默认跳过同名项目。复制始终回读并验证 SHA-256。");
                    if ui.button("字体许可").clicked() {
                        self.show_license = true;
                    }
                });
        }
        if self.show_license {
            egui::Window::new("字体许可")
                .open(&mut self.show_license)
                .collapsible(false)
                .default_width(460.0)
                .show(context, |ui| {
                    ui.label(include_str!("../assets/FONT-NOTICE.txt"));
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .show(ui, |ui| {
                            ui.label(include_str!("../assets/Apache-2.0.txt"));
                        });
                });
        }

        if let Some(report) = &self.report
            && self.show_report
        {
            let mut export = false;
            egui::Window::new("任务结果")
                .open(&mut self.show_report)
                .collapsible(false)
                .default_width(690.0)
                .show(context, |ui| {
                    if report.cancelled {
                        ui.weak("任务已取消，已完成的操作仍然有效。");
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(format!("清单基准目录  {}", self.report_base.display()));
                        export = ui
                            .add_enabled(
                                !report.hashes.is_empty(),
                                egui::Button::new("导出 .sha256"),
                            )
                            .clicked();
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(350.0).show_rows(
                        ui,
                        30.0,
                        report.rows.len(),
                        |ui, range| {
                            for index in range {
                                let row = &report.rows[index];
                                ui.horizontal(|ui| {
                                    match row.outcome {
                                        Outcome::Passed => {
                                            ui.strong("成功");
                                        }
                                        Outcome::Skipped => {
                                            ui.weak("跳过");
                                        }
                                        Outcome::Failed => {
                                            ui.colored_label(error_color(ui), "失败");
                                        }
                                    }
                                    let path = if row.path.as_os_str().is_empty() {
                                        "/".to_owned()
                                    } else {
                                        row.path.display().to_string()
                                    };
                                    let detail = format!("{path}  ·  {}", row.detail);
                                    if ui.small_button("复制详情").clicked() {
                                        context.copy_text(detail.clone());
                                    }
                                    if row.detail.len() == 64
                                        && jobs::parse_hash(&row.detail).is_some()
                                        && ui.small_button("复制哈希").clicked()
                                    {
                                        context.copy_text(row.detail.clone());
                                    }
                                    ui.add(egui::Label::new(&detail).truncate())
                                        .on_hover_text(&detail);
                                });
                            }
                        },
                    );
                });
            if export {
                self.path_input = Some((PathAction::ExportManifest, String::new()));
            }
        }
        self.path_dialog(context);
    }

    fn device_dialog(&mut self, context: &egui::Context) {
        let Some((operation, selected)) = &self.confirm_device else {
            return;
        };
        let operation = *operation;
        let same = self.current().is_some_and(|current| current.same(selected))
            && self.active.is_none()
            && self.browse.is_none();
        let formatting = operation == Maintenance::FormatExfat;
        let title = if formatting {
            "格式化为 exFAT？"
        } else {
            "安全弹出 U 盘？"
        };
        let response = egui::Modal::new(egui::Id::new("device_operation")).show(context, |ui| {
            ui.heading(title);
            ui.label(format!(
                "{}  ·  {}  ·  {}",
                selected.label,
                selected.mount.display(),
                selected.filesystem.to_uppercase()
            ));
            if formatting {
                ui.colored_label(
                    error_color(ui),
                    "所选卷上的文件将被删除。exFAT 支持超过 4 GiB 的单个文件。",
                );
                #[cfg(windows)]
                ui.weak("Windows 上需要以管理员身份启动本程序。");
                ui.label("输入 FORMAT 以确认。");
                ui.text_edit_singleline(&mut self.format_confirmation);
            } else {
                ui.label("将弹出整台 USB 设备。请等到系统确认后再拔出。");
            }
            ui.horizontal(|ui| {
                let cancel = ui.button("取消").clicked();
                let button = if formatting {
                    RichText::new("格式化").color(error_color(ui))
                } else {
                    RichText::new("安全弹出")
                };
                let start = ui
                    .add_enabled(
                        same && (!formatting || self.format_confirmation == "FORMAT"),
                        egui::Button::new(button),
                    )
                    .clicked();
                (cancel, start)
            })
            .inner
        });
        let (cancel, start) = response.inner;
        if start {
            let action = if formatting {
                "格式化"
            } else {
                "安全弹出"
            };
            self.start(context, action, Job::Device(operation));
        }
        if cancel || start || response.should_close() {
            self.confirm_device = None;
        }
    }

    fn path_dialog(&mut self, context: &egui::Context) {
        let Some((action, input)) = &mut self.path_input else {
            return;
        };
        let action = *action;
        let (heading, button) = match action {
            PathAction::AddSource => ("添加源路径", "添加"),
            PathAction::ImportManifest => ("导入校验清单", "开始校验"),
            PathAction::ExportManifest => ("导出校验清单", "保存"),
        };
        let response = egui::Modal::new(egui::Id::new("path")).show(context, |ui| {
            ui.heading(heading);
            ui.add(
                egui::TextEdit::singleline(input)
                    .desired_width(380.0)
                    .hint_text("输入绝对路径"),
            );
            ui.horizontal(|ui| {
                let cancel = ui.button("取消").clicked();
                let typed = Path::new(input.as_str());
                let mut chosen = ui
                    .add_enabled(typed.is_absolute(), egui::Button::new(button))
                    .clicked()
                    .then(|| typed.to_owned());
                if !matches!(action, PathAction::AddSource) && ui.button("浏览…").clicked() {
                    let dialog = rfd::FileDialog::new().add_filter("SHA-256", &["sha256", "txt"]);
                    chosen = match action {
                        PathAction::ImportManifest => dialog.pick_file(),
                        PathAction::ExportManifest => {
                            dialog.set_file_name("checksums.sha256").save_file()
                        }
                        PathAction::AddSource => None,
                    };
                }
                (cancel, chosen)
            })
            .inner
        });
        let close = response.should_close() || response.inner.0;
        if let Some(path) = response.inner.1 {
            self.path_input = None;
            match action {
                PathAction::AddSource => self.add_sources([path]),
                PathAction::ImportManifest => self.start(context, "清单校验", Job::Verify(path)),
                PathAction::ExportManifest => {
                    if let Some(report) = &self.report {
                        match jobs::export(&path, &report.hashes) {
                            Ok(()) => {
                                self.status = format!("校验清单已导出到 {}", path.display());
                                self.issue = false;
                            }
                            Err(error) => {
                                self.status = format!("无法导出校验清单：{error}");
                                self.issue = true;
                            }
                        }
                    }
                }
            }
        } else if close {
            self.path_input = None;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(context);
        if self.active.is_none() {
            let paths = context.input(|input| {
                input
                    .raw
                    .dropped_files
                    .iter()
                    .filter_map(|item| item.path.clone())
                    .collect::<Vec<_>>()
            });
            self.add_sources(paths);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::same(18)))
            .show_inside(ui, |ui| {
                self.header(ui);
                ui.separator();
                let reserved = if self.active.is_some() {
                    185.0
                } else if self.status.is_empty() {
                    119.0
                } else {
                    150.0
                };
                let height = (ui.available_height() - reserved).max(95.0);
                let idle = self.active.is_none();
                let ready = idle
                    && self.browse.is_none()
                    && self.current().is_some()
                    && self.listing.error.is_none();
                ui.columns(2, |columns| {
                    for column in columns.iter_mut() {
                        column.set_min_height(height + 74.0);
                    }
                    columns[0].add_enabled_ui(idle, |ui| self.sources(ui, height));
                    self.files(&mut columns[1], height, ready);
                });
                ui.separator();
                self.footer(ui, ready);
            });
        self.dialogs(&context);
    }
}

fn load_listing(mut volumes: Vec<Volume>, selected: Option<usize>, folder: PathBuf) -> Listing {
    let mut listing = Listing {
        volumes: Vec::new(),
        selected,
        folder,
        entries: Vec::new(),
        error: None,
    };
    if let Some(index) = selected {
        let volume = &mut volumes[index];
        let result = (|| {
            let session = Session::open(volume)?;
            let directory = session.folder(&listing.folder)?;
            let mut entries = Vec::new();
            for item in fs::read_dir(directory.path())? {
                let item = item?;
                let metadata = fs::symlink_metadata(item.path())?;
                entries.push(Entry {
                    name: item.file_name(),
                    directory: metadata.is_dir(),
                    link: usb_copy::copy::is_link(&metadata)
                        || !(metadata.is_file() || metadata.is_dir()),
                    bytes: metadata.len(),
                });
            }
            entries.sort_by(|a, b| {
                b.directory
                    .cmp(&a.directory)
                    .then_with(|| a.name.cmp(&b.name))
            });
            volume.available = session.available()?;
            Ok::<_, io::Error>(entries)
        })();
        match result {
            Ok(entries) => listing.entries = entries,
            Err(error) => listing.error = Some(format!("无法打开此目录：{error}")),
        }
    }
    listing.volumes = volumes;
    listing
}

fn size(bytes: u64) -> String {
    for (label, power) in [("TiB", 40), ("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        let unit = 1u64 << power;
        if bytes >= unit {
            return format!("{:.1} {label}", bytes as f64 / unit as f64);
        }
    }
    format!("{bytes} B")
}

fn error_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().error_fg_color
}
