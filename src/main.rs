#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use eframe::egui;
use egui::RichText;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

mod backup;

const DEFAULT_EXTENSIONS: &str = ".doc,.docx,.docm,.xls,.xlsx,.xlsm,.ppt,.pptx,.pptm,.pdf,.txt";

#[derive(Clone, Debug)]
struct FileRecord {
    path: PathBuf,
    created: DateTime<Local>,
    modified: DateTime<Local>,
    diff_days: i64,
    selected: bool,
}

#[derive(Debug)]
enum ScanMessage {
    Progress {
        scanned: usize,
        matched: usize,
        errors: usize,
    },
    Done {
        records: Vec<FileRecord>,
        errors: Vec<String>,
    },
}

#[derive(Debug)]
enum EditMessage {
    Progress {
        completed: usize,
        total: usize,
    },
    Done {
        changed: usize,
        failures: Vec<String>,
        action: &'static str,
    },
}

#[derive(Default)]
struct EditOptions {
    open: bool,
    time_text: String,
    sync_filesystem: bool,
    sync_office: bool,
    backup_directory: PathBuf,
}

struct App {
    roots: Vec<PathBuf>,
    extensions_text: String,
    min_diff_days: i64,
    exact_diff: bool,
    records: Vec<FileRecord>,
    errors: Vec<String>,
    status: String,
    scan_rx: Option<Receiver<ScanMessage>>,
    edit_rx: Option<Receiver<EditMessage>>,
    edit_options: EditOptions,
    show_errors: bool,
    restore_preview: Option<(PathBuf, backup::BackupRecord)>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            extensions_text: DEFAULT_EXTENSIONS.to_string(),
            min_diff_days: 0,
            exact_diff: false,
            records: Vec::new(),
            errors: Vec::new(),
            status: "请选择要扫描的文件夹或磁盘，然后点击“开始扫描”。".to_string(),
            scan_rx: None,
            edit_rx: None,
            edit_options: EditOptions {
                sync_filesystem: true,
                sync_office: true,
                backup_directory: backup::default_directory(),
                ..Default::default()
            },
            show_errors: false,
            restore_preview: None,
        }
    }
}

impl App {
    fn add_folder(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            if !self.roots.contains(&folder) {
                self.roots.push(folder);
            }
        }
    }

    fn add_drives(&mut self) {
        #[cfg(windows)]
        {
            for letter in b'A'..=b'Z' {
                let root = PathBuf::from(format!("{}:\\", letter as char));
                if root.exists() && !self.roots.contains(&root) {
                    self.roots.push(root);
                }
            }
        }
        #[cfg(not(windows))]
        {
            if !self.roots.iter().any(|r| r == Path::new("/")) {
                self.roots.push(PathBuf::from("/"));
            }
        }
    }

    fn start_scan(&mut self) {
        if self.scan_rx.is_some() || self.edit_rx.is_some() {
            return;
        }
        if self.roots.is_empty() {
            self.status = "请先添加至少一个文件夹或磁盘。".to_string();
            return;
        }
        let extensions = parse_extensions(&self.extensions_text);
        if extensions.is_empty() {
            self.status = "请至少填写一个文件后缀，例如 .docx,.xlsx。".to_string();
            return;
        }
        let roots = self.roots.clone();
        let min_diff_days = self.min_diff_days.max(0);
        let exact_diff = self.exact_diff;
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        self.records.clear();
        self.errors.clear();
        self.status = "正在扫描……".to_string();
        thread::spawn(move || scan_files(roots, extensions, min_diff_days, exact_diff, tx));
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.records
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.path.clone())
            .collect()
    }

    fn open_edit_dialog(&mut self) {
        let count = self.selected_paths().len();
        if count == 0 {
            self.status = "请先在结果列表中选择文件。".to_string();
            return;
        }
        self.edit_options.open = true;
        if self.edit_options.time_text.is_empty() {
            self.edit_options.time_text = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        }
    }

    fn start_edit(&mut self) {
        if self.edit_rx.is_some() || self.scan_rx.is_some() {
            return;
        }
        let selected = self.selected_paths();
        if selected.is_empty() {
            self.status = "没有选择文件。".to_string();
            return;
        }
        let time = match parse_local_time(&self.edit_options.time_text) {
            Ok(value) => value,
            Err(message) => {
                self.status = message;
                return;
            }
        };
        if !self.edit_options.sync_filesystem && !self.edit_options.sync_office {
            self.status = "至少勾选一种要同步的时间。".to_string();
            return;
        }
        if let Err(error) = backup::validate_directory(&self.edit_options.backup_directory) {
            self.status = format!("无法开始修改：{error:#}");
            return;
        }
        let options = EditWorkerOptions {
            sync_filesystem: self.edit_options.sync_filesystem,
            sync_office: self.edit_options.sync_office,
            backup_directory: self.edit_options.backup_directory.clone(),
            time,
        };
        let total = selected.len();
        let (tx, rx) = mpsc::channel();
        self.edit_rx = Some(rx);
        self.edit_options.open = false;
        self.status = format!("正在修改 0/{total} 个文件……");
        thread::spawn(move || edit_files(selected, options, tx));
    }

    fn choose_restore(&mut self) {
        if let Some(manifest) = rfd::FileDialog::new()
            .set_directory(&self.edit_options.backup_directory)
            .add_filter("备份记录", &["json"])
            .pick_file()
        {
            match backup::load(&manifest) {
                Ok(backup) => self.restore_preview = Some((manifest, backup.record)),
                Err(error) => self.status = format!("无法读取备份：{error:#}"),
            }
        }
    }

    fn start_restore(&mut self, manifest: PathBuf) {
        if self.scan_rx.is_some() || self.edit_rx.is_some() {
            return;
        }
        self.restore_preview = None;
        let (tx, rx) = mpsc::channel();
        self.edit_rx = Some(rx);
        self.status = "正在校验备份并回滚……".to_string();
        thread::spawn(move || {
            let (changed, failures) = match backup::restore(&manifest) {
                Ok(()) => (1, Vec::new()),
                Err(error) => (0, vec![format!("{error:#}")]),
            };
            let _ = tx.send(EditMessage::Done {
                changed,
                failures,
                action: "回滚",
            });
        });
    }

    fn poll_workers(&mut self) {
        if let Some(rx) = self.scan_rx.take() {
            let mut finished = false;
            while let Ok(message) = rx.try_recv() {
                match message {
                    ScanMessage::Progress {
                        scanned,
                        matched,
                        errors,
                    } => {
                        self.status =
                            format!("扫描中：已检查 {scanned}，匹配 {matched}，跳过/失败 {errors}");
                    }
                    ScanMessage::Done { records, errors } => {
                        self.records = records;
                        self.errors = errors;
                        self.status =
                            format!("扫描完成：找到 {} 个符合条件的文件。", self.records.len());
                        finished = true;
                    }
                }
            }
            if !finished {
                self.scan_rx = Some(rx);
            }
        }
        if let Some(rx) = self.edit_rx.take() {
            let mut finished = false;
            while let Ok(message) = rx.try_recv() {
                match message {
                    EditMessage::Progress { completed, total } => {
                        self.status = format!("正在修改 {completed}/{total} 个文件……");
                    }
                    EditMessage::Done {
                        changed,
                        failures,
                        action,
                    } => {
                        self.errors = failures;
                        self.status = format!(
                            "{action}完成：成功 {changed} 个，失败 {} 个。请重新扫描确认。",
                            self.errors.len()
                        );
                        finished = true;
                    }
                }
            }
            if !finished {
                self.edit_rx = Some(rx);
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_workers();
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        let busy = self.scan_rx.is_some() || self.edit_rx.is_some();
        let selected_count = self.records.iter().filter(|record| record.selected).count();

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(18.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("360").size(26.0).strong().color(ACCENT));
                });
            });

        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::WHITE)
                    .inner_margin(12.0),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if busy {
                        ui.spinner();
                    }
                    ui.label(&self.status);
                    if !self.errors.is_empty()
                        && ui
                            .button(format!("查看错误 ({})", self.errors.len()))
                            .clicked()
                    {
                        self.show_errors = true;
                    }
                });
            });

        egui::SidePanel::left("settings")
            .resizable(true)
            .default_width(280.0)
            .width_range(260.0..=360.0)
            .frame(egui::Frame::none().fill(egui::Color32::WHITE).inner_margin(18.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("扫描设置");
                    ui.add_space(16.0);
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label(RichText::new("扫描位置").strong());
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            if ui.button("添加文件夹").clicked() {
                                self.add_folder();
                            }
                            if ui.button("添加全部磁盘").clicked() {
                                self.add_drives();
                            }
                        });
                        egui::Frame::none()
                            .fill(BACKGROUND)
                            .rounding(6.0)
                            .inner_margin(10.0)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                if self.roots.is_empty() {
                                    ui.label(RichText::new("尚未添加扫描位置").color(MUTED));
                                } else {
                                    egui::ScrollArea::vertical().id_salt("roots").max_height(120.0).show(ui, |ui| {
                                        for root in &self.roots {
                                            ui.add(egui::Label::new(root.to_string_lossy()).wrap());
                                        }
                                    });
                                }
                            });
                        if !self.roots.is_empty() && ui.small_button("清空位置").clicked() {
                            self.roots.clear();
                        }
                        ui.add_space(16.0);
                        ui.label(RichText::new("文件类型").strong());
                        ui.add(egui::TextEdit::multiline(&mut self.extensions_text)
                            .desired_width(f32::INFINITY).desired_rows(3));
                        ui.label(RichText::new("后缀用逗号分隔，可添加 .jpg、.zip 等类型。").small().color(MUTED));
                        ui.add_space(16.0);
                        ui.label(RichText::new("时间差异").strong());
                        ui.horizontal(|ui| {
                            ui.label(if self.exact_diff { "恰好相差" } else { "至少相差" });
                            ui.add(egui::DragValue::new(&mut self.min_diff_days).range(0..=3650).suffix(" 天"));
                        });
                        ui.checkbox(&mut self.exact_diff, "只显示恰好相差的文件");
                        ui.add_space(16.0);
                        if ui.add_sized([ui.available_width(), 36.0], primary_button("开始扫描")).clicked() {
                            self.start_scan();
                        }
                    });
                    ui.add_space(16.0);
                    ui.separator();
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label(RichText::new("U 盘备份目录（必需）").strong());
                        ui.add(egui::Label::new(self.edit_options.backup_directory.to_string_lossy()).wrap());
                        if ui.button("选择 U 盘备份位置").clicked() {
                            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                self.edit_options.backup_directory = folder.join("360-backups");
                            }
                        }
                        if ui.button("从备份回滚…").clicked() { self.choose_restore(); }
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("修改前必须在 U 盘备份，备份失败不会修改。").small().color(MUTED));
                    ui.add_space(4.0);
                    ui.collapsing("支持的文件与修改说明", |ui| {
                        ui.label("默认扫描 Word、Excel、PowerPoint、PDF、TXT。");
                        ui.label("docx/docm、xlsx/xlsm、pptx/pptm 支持同步 Office 内部属性；旧版 doc/xls/ppt 仅修改文件系统时间。");
                        ui.label("备份目录包含 original.bin（原内容）和 manifest.json（时间、属性、校验值等）。");
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BACKGROUND).inner_margin(20.0))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.heading("文件结果");
                    ui.label(
                        RichText::new(format!(
                            "共 {} 个 · 已选 {} 个",
                            self.records.len(),
                            selected_count
                        ))
                        .color(MUTED),
                    );
                });
                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            !busy && selected_count > 0,
                            primary_button("修改选中文件时间"),
                        )
                        .clicked()
                    {
                        self.open_edit_dialog();
                    }
                    if ui
                        .add_enabled(!busy && !self.records.is_empty(), egui::Button::new("全选"))
                        .clicked()
                    {
                        for record in &mut self.records {
                            record.selected = true;
                        }
                    }
                    if ui
                        .add_enabled(!busy && selected_count > 0, egui::Button::new("取消全选"))
                        .clicked()
                    {
                        for record in &mut self.records {
                            record.selected = false;
                        }
                    }
                });
                ui.add_space(12.0);
                egui::Frame::none()
                    .fill(egui::Color32::WHITE)
                    .stroke(egui::Stroke::new(1.0, BORDER))
                    .rounding(8.0)
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.set_min_size(ui.available_size());
                        if self.records.is_empty() {
                            ui.add_space((ui.available_height() * 0.30).max(24.0));
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(if busy {
                                        "正在扫描文件…"
                                    } else {
                                        "暂无文件结果"
                                    })
                                    .size(19.0)
                                    .strong(),
                                );
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(if busy {
                                        "扫描完成后将在这里显示匹配的文件。"
                                    } else {
                                        "在左侧添加扫描位置，设置条件后开始扫描。"
                                    })
                                    .color(MUTED),
                                );
                            });
                        } else {
                            egui::ScrollArea::both()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.add_enabled_ui(!busy, |ui| {
                                        egui::Grid::new("results")
                                            .striped(true)
                                            .spacing([20.0, 10.0])
                                            .min_row_height(30.0)
                                            .show(ui, |ui| {
                                                for title in [
                                                    "选择",
                                                    "文件",
                                                    "创建时间",
                                                    "最后修改",
                                                    "相差天数",
                                                ] {
                                                    ui.label(
                                                        RichText::new(title).strong().color(MUTED),
                                                    );
                                                }
                                                ui.end_row();
                                                for record in &mut self.records {
                                                    ui.checkbox(&mut record.selected, "");
                                                    ui.vertical(|ui| {
                                                        ui.set_width(240.0);
                                                        let name = record
                                                            .path
                                                            .file_name()
                                                            .unwrap_or(record.path.as_os_str());
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(
                                                                    name.to_string_lossy(),
                                                                )
                                                                .strong(),
                                                            )
                                                            .truncate(),
                                                        )
                                                        .on_hover_text(
                                                            record.path.to_string_lossy(),
                                                        );
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(
                                                                    record.path.to_string_lossy(),
                                                                )
                                                                .small()
                                                                .color(MUTED),
                                                            )
                                                            .truncate(),
                                                        )
                                                        .on_hover_text(
                                                            record.path.to_string_lossy(),
                                                        );
                                                    });
                                                    ui.label(
                                                        record
                                                            .created
                                                            .format("%Y-%m-%d %H:%M:%S")
                                                            .to_string(),
                                                    );
                                                    ui.label(
                                                        record
                                                            .modified
                                                            .format("%Y-%m-%d %H:%M:%S")
                                                            .to_string(),
                                                    );
                                                    ui.label(format!("{} 天", record.diff_days));
                                                    ui.end_row();
                                                }
                                            });
                                    });
                                });
                        }
                    });
            });

        if self.edit_options.open {
            egui::Window::new("同步文件时间")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(460.0)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "将修改 {} 个选中文件。",
                        self.selected_paths().len()
                    ));
                    ui.label("目标时间（本机当地时间）");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.edit_options.time_text)
                            .desired_width(f32::INFINITY)
                            .hint_text("YYYY-MM-DD HH:MM:SS"),
                    );
                    ui.label(
                        RichText::new("格式：2026-10-16 14:30:00")
                            .small()
                            .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut self.edit_options.sync_filesystem,
                        "同步 Windows 文件创建/访问/最后写入时间",
                    );
                    ui.checkbox(
                        &mut self.edit_options.sync_office,
                        "同步 Office 内部 created/modified（仅 docx/xlsx/pptx 等 OOXML）",
                    );
                    ui.label("强制备份原文件和元数据，校验成功后才会修改。");
                    ui.add(
                        egui::Label::new(format!(
                            "备份位置：{}",
                            self.edit_options.backup_directory.display()
                        ))
                        .wrap(),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("取消").clicked() {
                            self.edit_options.open = false;
                        }
                        if ui.add(primary_button("开始修改")).clicked() {
                            self.start_edit();
                        }
                    });
                });
        }

        if let Some((manifest, record)) = self.restore_preview.clone() {
            egui::Window::new("确认从 U 盘备份回滚")
                .collapsible(false)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.label(format!("原文件：{}", record.original_path.display()));
                    ui.label(format!("备份时间：{}", record.saved_at_utc));
                    ui.label(format!("原大小：{} 字节", record.metadata.size));
                    ui.label(format!(
                        "原修改时间：{}",
                        DateTime::<Local>::from(record.metadata.modified)
                            .format("%Y-%m-%d %H:%M:%S%.f")
                    ));
                    ui.label("将先备份当前版本，再恢复所选备份的内容、时间和属性。");
                    ui.label("原文件必须仍在原路径且身份一致；回滚前会校验备份完整性。");
                    ui.horizontal(|ui| {
                        if ui.button("取消").clicked() {
                            self.restore_preview = None;
                        }
                        if ui.add_enabled(!busy, primary_button("确认回滚")).clicked() {
                            self.start_restore(manifest);
                        }
                    });
                });
        }

        if self.show_errors {
            egui::Window::new("扫描/修改错误")
                .open(&mut self.show_errors)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(400.0)
                        .show(ui, |ui| {
                            for error in &self.errors {
                                ui.label(error);
                            }
                        });
                });
        }
    }
}

fn parse_extensions(input: &str) -> HashSet<String> {
    input
        .split([',', ';', '\n', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            if lower.starts_with('.') {
                lower
            } else {
                format!(".{lower}")
            }
        })
        .collect()
}

fn scan_files(
    roots: Vec<PathBuf>,
    extensions: HashSet<String>,
    min_diff_days: i64,
    exact_diff: bool,
    tx: Sender<ScanMessage>,
) {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    let mut scanned = 0;
    let mut seen = HashSet::new();
    for root in roots {
        for entry_result in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                !entry.file_type().is_dir()
                    || (entry.file_name() != "360-backups"
                        && !(entry.path().join(backup::MANIFEST_NAME).is_file()
                            && entry.path().join("original.bin").is_file()))
            })
        {
            let entry = match entry_result {
                Ok(value) => value,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let identity = match fs::canonicalize(entry.path()) {
                Ok(path) => path,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if !seen.insert(identity) {
                continue;
            }
            scanned += 1;
            let extension = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}").to_ascii_lowercase());
            if extension
                .as_deref()
                .map(|e| !extensions.contains(e))
                .unwrap_or(true)
            {
                continue;
            }
            match fs::metadata(entry.path()).and_then(|meta| {
                let created = meta.created()?;
                let modified = meta.modified()?;
                Ok((created, modified))
            }) {
                Ok((created, modified)) => {
                    let created: DateTime<Local> = created.into();
                    let modified: DateTime<Local> = modified.into();
                    let diff_days = (modified.timestamp() - created.timestamp()).abs() / 86_400;
                    if (exact_diff && diff_days == min_diff_days)
                        || (!exact_diff && diff_days >= min_diff_days)
                    {
                        records.push(FileRecord {
                            path: entry.path().to_path_buf(),
                            created,
                            modified,
                            diff_days,
                            selected: false,
                        });
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", entry.path().display())),
            }
            if scanned % 100 == 0 {
                let _ = tx.send(ScanMessage::Progress {
                    scanned,
                    matched: records.len(),
                    errors: errors.len(),
                });
            }
        }
    }
    let _ = tx.send(ScanMessage::Done { records, errors });
}

struct EditWorkerOptions {
    sync_filesystem: bool,
    sync_office: bool,
    backup_directory: PathBuf,
    time: DateTime<Local>,
}

fn edit_files(paths: Vec<PathBuf>, options: EditWorkerOptions, tx: Sender<EditMessage>) {
    let total = paths.len();
    let mut changed = 0;
    let mut failures = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        match update_one(path, &options) {
            Ok(()) => changed += 1,
            Err(error) => failures.push(format!("{}: {error:#}", path.display())),
        }
        let _ = tx.send(EditMessage::Progress {
            completed: index + 1,
            total,
        });
    }
    let _ = tx.send(EditMessage::Done {
        changed,
        failures,
        action: "修改",
    });
}

fn update_one(path: &Path, options: &EditWorkerOptions) -> Result<()> {
    if !options.sync_filesystem && (!options.sync_office || !is_ooxml(path)) {
        return Err(anyhow!("此文件不支持 Office 内部时间同步，未修改"));
    }
    backup::validate_directory(&options.backup_directory)?;
    let mut file = backup::open_original(path)?;
    let saved = backup::create(&mut file, path, &options.backup_directory)?;
    let mut payload = saved.verified_payload()?;
    let prepare = (|| -> Result<_> {
        if options.sync_office && is_ooxml(path) {
            let mut staged = tempfile::NamedTempFile::new_in(&saved.directory)?;
            rewrite_core_properties(
                &saved.directory.join("original.bin"),
                staged.as_file_mut(),
                options.time.with_timezone(&Utc),
            )?;
            staged.as_file().sync_all()?;
            Ok(Some(staged))
        } else {
            Ok(None)
        }
    })();
    let mut staged =
        prepare.with_context(|| format!("未修改原文件；备份位于 {}", saved.directory.display()))?;
    backup::apply_with_recovery(&saved, &mut file, &mut payload, |file| {
        if let Some(staged) = &mut staged {
            let expected = backup::digest(staged.as_file_mut())?;
            backup::write_in_place(staged.as_file_mut(), file)?;
            if backup::digest(file)? != expected {
                return Err(anyhow!("写回内容校验失败"));
            }
        }
        if options.sync_filesystem {
            saved
                .record
                .metadata
                .set_target_times(file, options.time.into())?;
        } else {
            // Office-only edits must not accidentally change filesystem timestamps.
            saved.record.metadata.restore(file)?;
            let mut expected = saved.record.metadata.clone();
            expected.size = file.metadata()?.len();
            expected.verify_restored(file)?;
        }
        Ok(())
    })
}

fn is_ooxml(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm")
    )
}

fn rewrite_core_properties(source: &Path, target: &mut File, time: DateTime<Utc>) -> Result<()> {
    let mut archive =
        ZipArchive::new(File::open(source)?).context("文件不是有效的 OOXML ZIP 包")?;
    let mut writer = ZipWriter::new(target);
    let mut found_core = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.name() == "docProps/core.xml" {
            let mut xml = String::new();
            entry
                .read_to_string(&mut xml)
                .context("core.xml 不是 UTF-8")?;
            let updated = replace_core_timestamps(&xml, &time);
            let expected = time.to_rfc3339_opts(SecondsFormat::Secs, true);
            // Refuse unsupported/missing fields instead of reporting a silent success.
            for tag in ["dcterms:created", "dcterms:modified"] {
                if !updated.contains(&format!(">{expected}</{tag}>")) {
                    return Err(anyhow!(
                        "Office 内部时间字段缺失或格式不支持：{tag}，原文件未修改"
                    ));
                }
            }
            writer.start_file(
                "docProps/core.xml",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )?;
            writer.write_all(updated.as_bytes())?;
            found_core = true;
        } else {
            writer.raw_copy_file(entry)?;
        }
    }
    if !found_core {
        return Err(anyhow!("缺少 docProps/core.xml，原文件未修改"));
    }
    writer.finish()?;
    Ok(())
}

fn replace_core_timestamps(xml: &str, time: &DateTime<Utc>) -> String {
    let value = time.to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut output = xml.to_string();
    output = replace_xml_element(&output, "dcterms:created", &value);
    replace_xml_element(&output, "dcterms:modified", &value)
}

fn replace_xml_element(xml: &str, tag: &str, value: &str) -> String {
    let open_start = match xml.find(&format!("<{tag}")) {
        Some(index) => index,
        None => return xml.to_string(),
    };
    let open_end = match xml[open_start..].find('>') {
        Some(offset) => open_start + offset,
        None => return xml.to_string(),
    };
    let open = &xml[open_start..=open_end];
    let close_tag = format!("</{tag}>");
    let end = if open.ends_with("/>") {
        open_end + 1
    } else {
        match xml[open_end + 1..].find(&close_tag) {
            Some(offset) => open_end + 1 + offset + close_tag.len(),
            None => return xml.to_string(),
        }
    };
    let replacement = if open.ends_with("/>") {
        format!("{}{}{}", open.trim_end_matches("/>").trim_end(), ">", value) + &close_tag
    } else {
        format!("{}>{}{close_tag}", open.trim_end_matches('>'), value)
    };
    format!("{}{}{}", &xml[..open_start], replacement, &xml[end..])
}

fn parse_local_time(value: &str) -> Result<DateTime<Local>, String> {
    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %H:%M:%S")
        .map_err(|_| "时间格式应为 YYYY-MM-DD HH:MM:SS，例如 2026-10-15 14:30:00。".to_string())?;
    Local
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| "目标时间在本机时区中不唯一。".to_string())
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "360",
        options,
        Box::new(|cc| {
            configure_fonts(&cc.egui_ctx);
            configure_style(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(38, 99, 205);
const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(245, 247, 250);
const BORDER: egui::Color32 = egui::Color32::from_rgb(221, 227, 235);
const MUTED: egui::Color32 = egui::Color32::from_rgb(100, 113, 132);

fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).color(egui::Color32::WHITE))
        .fill(ACCENT)
        .stroke(egui::Stroke::NONE)
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.override_text_color = Some(egui::Color32::from_rgb(36, 47, 64));
    style.visuals.panel_fill = egui::Color32::WHITE;
    style.visuals.window_fill = egui::Color32::WHITE;
    style.visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.window_rounding = egui::Rounding::same(10.0);
    style.visuals.faint_bg_color = BACKGROUND;
    style.visuals.extreme_bg_color = egui::Color32::WHITE;
    style.visuals.selection.bg_fill = egui::Color32::from_rgb(221, 234, 255);
    style.visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.bg_fill = BACKGROUND;
    style.visuals.widgets.inactive.weak_bg_fill = BACKGROUND;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(234, 240, 251);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(234, 240, 251);
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(221, 234, 255);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(221, 234, 255);
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.window_margin = egui::Margin::same(20.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(19.0));
    ctx.set_style(style);
}

fn configure_fonts(ctx: &egui::Context) {
    let candidates = if cfg!(target_os = "macos") {
        vec![
            "/System/Library/AssetsV2/com_apple_MobileAsset_Font8/53fe5be564086fefc7523ccd0a31200acf92e0e5.asset/AssetData/STHEITI.ttf",
            "/System/Library/AssetsV2/com_apple_MobileAsset_Font8/5d3a1229823fff781c0d9dac611c90f9fc31e138.asset/AssetData/LiHei Pro.ttf",
            "/System/Library/Fonts/Supplemental/NISC18030.ttf",
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\msyh.ttf",
            r"C:\Windows\Fonts\simhei.ttf",
        ]
    } else {
        vec![
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        ]
    };
    let Some(path) = candidates
        .into_iter()
        .map(Path::new)
        .find(|path| path.exists())
    else {
        return;
    };
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_string(), egui::FontData::from_owned(bytes));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(family_fonts) = fonts.families.get_mut(&family) {
            family_fonts.insert(0, "cjk".to_string());
        }
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_normalized() {
        let result = parse_extensions("docx, .XLSX;txt");
        assert!(result.contains(".docx"));
        assert!(result.contains(".xlsx"));
        assert!(result.contains(".txt"));
    }

    #[test]
    fn core_timestamps_are_replaced() {
        let xml = r#"<cp:coreProperties><dcterms:created xsi:type="dcterms:W3CDTF">2020-01-01T00:00:00Z</dcterms:created><dcterms:modified xsi:type="dcterms:W3CDTF">2020-01-02T00:00:00Z</dcterms:modified></cp:coreProperties>"#;
        let time = DateTime::parse_from_rfc3339("2026-10-15T06:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let result = replace_core_timestamps(xml, &time);
        assert!(result.contains("2026-10-15T06:30:00Z"));
        assert_eq!(result.matches("2026-10-15T06:30:00Z").count(), 2);
    }

    #[test]
    fn ooxml_rewrite_preserves_other_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sample.docx");
        {
            let file = File::create(&path).unwrap();
            let mut writer = ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            writer.start_file("docProps/core.xml", options).unwrap();
            writer.write_all(br#"<cp:coreProperties><dcterms:created>2020-01-01T00:00:00Z</dcterms:created><dcterms:modified>2020-01-01T00:00:00Z</dcterms:modified></cp:coreProperties>"#).unwrap();
            writer.start_file("word/document.xml", options).unwrap();
            writer.write_all(b"document-body").unwrap();
            writer.finish().unwrap();
        }
        let time = DateTime::parse_from_rfc3339("2026-10-15T06:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut output = tempfile::tempfile().unwrap();
        rewrite_core_properties(&path, &mut output, time).unwrap();
        let mut archive = ZipArchive::new(output).unwrap();
        let mut core = String::new();
        archive
            .by_name("docProps/core.xml")
            .unwrap()
            .read_to_string(&mut core)
            .unwrap();
        assert!(core.contains("2026-10-15T06:30:00Z"));
        let mut document = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut document)
            .unwrap();
        assert_eq!(document, "document-body");
    }
    #[cfg(not(windows))]
    #[test]
    fn office_edit_and_manual_rollback_keep_source_directory_clean() {
        let source = tempfile::tempdir().unwrap();
        let usb = tempfile::tempdir().unwrap();
        let path = source.path().join("sample.docx");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("docProps/core.xml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(br#"<cp:coreProperties><dcterms:created>2020-01-01T00:00:00Z</dcterms:created><dcterms:modified>2020-01-01T00:00:00Z</dcterms:modified></cp:coreProperties>"#).unwrap();
        zip.start_file("word/document.xml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"keep original document").unwrap();
        zip.finish().unwrap();
        let mut original = File::open(&path).unwrap();
        let before_hash = backup::digest(&mut original).unwrap();
        let before = backup::FileSnapshot::capture(&original).unwrap();
        drop(original);
        // macOS may move birth time backwards when mtime predates birth time.
        // Windows explicitly sets/restores birth time; use a future target for this portable test.
        let target = (Utc::now() + chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        update_one(
            &path,
            &EditWorkerOptions {
                sync_filesystem: true,
                sync_office: true,
                backup_directory: usb.path().to_path_buf(),
                time: DateTime::parse_from_rfc3339(&target)
                    .unwrap()
                    .with_timezone(&Local),
            },
        )
        .unwrap();
        assert_eq!(fs::read_dir(source.path()).unwrap().count(), 1);
        let manifest = fs::read_dir(usb.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(backup::MANIFEST_NAME);
        let mut archive = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut core = String::new();
        archive
            .by_name("docProps/core.xml")
            .unwrap()
            .read_to_string(&mut core)
            .unwrap();
        assert_eq!(core.matches(&target).count(), 2);
        drop(archive);
        backup::restore(&manifest).unwrap();
        let mut restored = File::open(&path).unwrap();
        before.verify_restored(&restored).unwrap();
        assert_eq!(backup::digest(&mut restored).unwrap(), before_hash);
        assert_eq!(fs::read_dir(source.path()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(usb.path()).unwrap().count(), 2);
    }

    #[cfg(not(windows))]
    #[test]
    fn office_prepare_failure_leaves_original_and_durable_backup_intact() {
        let source = tempfile::tempdir().unwrap();
        let usb = tempfile::tempdir().unwrap();
        let path = source.path().join("invalid.docx");
        fs::write(&path, b"invalid office data").unwrap();
        let before = backup::FileSnapshot::capture(&File::open(&path).unwrap()).unwrap();
        let result = update_one(
            &path,
            &EditWorkerOptions {
                sync_filesystem: true,
                sync_office: true,
                backup_directory: usb.path().to_path_buf(),
                time: Local::now(),
            },
        );
        assert!(result.is_err());
        before.verify_restored(&File::open(&path).unwrap()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"invalid office data");
        let manifest = fs::read_dir(usb.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join(backup::MANIFEST_NAME);
        backup::load(&manifest).unwrap().verified_payload().unwrap();
        assert_eq!(fs::read_dir(source.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_office_timestamp_is_rejected_before_writeback() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("missing.docx");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("docProps/core.xml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"<cp:coreProperties/>").unwrap();
        zip.finish().unwrap();
        let before = fs::read(&path).unwrap();
        let mut target = tempfile::tempfile().unwrap();
        assert!(rewrite_core_properties(&path, &mut target, Utc::now()).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
