//! Durable USB backups. Originals are opened without truncation and never unlinked.
use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const MANIFEST_NAME: &str = "manifest.json";
const PAYLOAD_NAME: &str = "original.bin";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSnapshot {
    pub size: u64,
    pub created: Option<SystemTime>,
    pub modified: SystemTime,
    pub accessed: SystemTime,
    pub readonly: bool,
    #[cfg(windows)]
    pub windows: native::WindowsSnapshot,
    #[cfg(unix)]
    pub device: u64,
    #[cfg(unix)]
    pub inode: u64,
    #[cfg(unix)]
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub version: u32,
    pub original_path: PathBuf,
    pub saved_at_utc: String,
    pub sha256: String,
    pub metadata: FileSnapshot,
    #[cfg(windows)]
    pub windows_streams_sha256: String,
}

pub struct Backup {
    pub directory: PathBuf,
    pub record: BackupRecord,
}

pub fn default_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("360-backups")))
        .unwrap_or_else(|| PathBuf::from("360-backups"))
}

/// Validate before creating anything. The UI always chooses an existing parent.
pub fn validate_directory(directory: &Path) -> Result<()> {
    ensure!(directory.is_absolute(), "请选择 U 盘上的绝对路径");
    let existing = directory
        .ancestors()
        .find(|p| p.exists())
        .context("备份目录不存在")?;
    ensure!(existing.is_dir(), "备份路径不是目录");
    #[cfg(windows)]
    native::require_removable_volume(&fs::canonicalize(existing)?)?;
    Ok(())
}

pub fn open_original(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path).context("原文件不存在")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "不支持目录或符号链接"
    );
    ensure!(
        !metadata.permissions().readonly(),
        "原文件为只读；未修改，请先确认是否允许编辑"
    );
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        // Deny concurrent readers, writers and deletion for the entire transaction.
        options
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .context("无法独占打开原文件，请关闭 Office/WPS 并检查权限")?;
    #[cfg(windows)]
    {
        native::ensure_regular(&file)?;
        native::freeze_automatic_times(&file)?;
    }
    Ok(file)
}

impl FileSnapshot {
    pub fn capture(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: metadata.len(),
            created: metadata.created().ok(),
            modified: metadata.modified()?,
            accessed: metadata.accessed()?,
            readonly: metadata.permissions().readonly(),
            #[cfg(windows)]
            windows: native::WindowsSnapshot::capture(file)?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
        })
    }

    pub fn ensure_same_file(&self, file: &File) -> Result<()> {
        let current = Self::capture(file)?;
        #[cfg(windows)]
        ensure!(
            self.windows.same_identity(&current.windows),
            "原路径已不是备份时的文件（磁盘或文件标识不同），已拒绝覆盖"
        );
        #[cfg(windows)]
        ensure!(
            self.windows.security == current.windows.security
                && self.windows.hard_links == current.windows.hard_links,
            "文件权限或硬链接已被外部修改，无法保证原样恢复，已拒绝覆盖"
        );
        #[cfg(unix)]
        ensure!(
            self.device == current.device && self.inode == current.inode,
            "原路径已不是备份时的文件，已拒绝覆盖"
        );
        Ok(())
    }

    pub fn restore(&self, file: &File) -> Result<()> {
        #[cfg(windows)]
        self.windows.restore(file)?;
        #[cfg(not(windows))]
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(self.mode))?;
            }
            file.set_times(
                FileTimes::new()
                    .set_accessed(self.accessed)
                    .set_modified(self.modified),
            )?;
        }
        Ok(())
    }

    pub fn verify_restored(&self, file: &File) -> Result<()> {
        let current = Self::capture(file)?;
        ensure!(
            current == *self,
            "文件内容已写回，但元数据读回与原记录不一致；备份仍保留，请勿视为完整恢复"
        );
        Ok(())
    }

    pub fn set_target_times(&self, file: &File, time: SystemTime) -> Result<()> {
        #[cfg(windows)]
        self.windows.set_target_times(file, time)?;
        #[cfg(not(windows))]
        file.set_times(FileTimes::new().set_accessed(time).set_modified(time))?;
        let current = Self::capture(file)?;
        ensure!(
            current.accessed == time && current.modified == time,
            "文件系统时间写入后校验不一致"
        );
        #[cfg(windows)]
        {
            ensure!(current.created == Some(time), "创建时间写入后校验不一致");
            ensure!(
                self.windows.same_identity(&current.windows)
                    && self.windows.attributes == current.windows.attributes
                    && self.windows.security == current.windows.security,
                "文件标识、属性或权限发生意外变化"
            );
        }
        Ok(())
    }
}

pub fn digest(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn create(file: &mut File, path: &Path, directory: &Path) -> Result<Backup> {
    create_impl(file, path, directory, validate_directory)
}

fn create_impl(
    file: &mut File,
    path: &Path,
    directory: &Path,
    validate: impl Fn(&Path) -> Result<()>,
) -> Result<Backup> {
    ensure!(path.is_absolute(), "原文件路径必须是绝对路径");
    validate(directory)?;
    let metadata = FileSnapshot::capture(file)?;
    let result = (|| -> Result<Backup> {
        fs::create_dir_all(directory).context("无法创建 U 盘备份目录")?;
        let directory = fs::canonicalize(directory)?;
        validate(&directory)?;
        #[cfg(windows)]
        let path = native::file_path(file)?;
        #[cfg(not(windows))]
        let path = fs::canonicalize(path)?;

        ensure!(!path.starts_with(&directory), "不允许修改备份目录内的文件");
        let folder = tempfile::Builder::new()
            .prefix("backup-")
            .tempdir_in(&directory)?;
        let mut payload = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(folder.path().join(PAYLOAD_NAME))?;
        file.seek(SeekFrom::Start(0))?;
        let copied = std::io::copy(file, &mut payload).context("备份写入失败，原文件未修改")?;
        payload.sync_all()?;
        let hash = digest(&mut payload)?;
        ensure!(
            copied == metadata.size && hash == digest(file)?,
            "备份内容与原文件不一致，已停止修改"
        );
        #[cfg(windows)]
        let windows_streams_sha256 = {
            let mut streams = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(folder.path().join("windows-streams.bin"))?;
            native::backup_streams(file, &mut streams)?;
            streams.sync_all()?;
            digest(&mut streams)?
        };
        let record = BackupRecord {
            version: 1,
            original_path: path,
            saved_at_utc: chrono::Utc::now().to_rfc3339(),
            sha256: hash,
            metadata: metadata.clone(),
            #[cfg(windows)]
            windows_streams_sha256,
        };
        let mut manifest = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(folder.path().join(MANIFEST_NAME))?;
        manifest.write_all(&serde_json::to_vec_pretty(&record)?)?;
        manifest.sync_all()?;
        drop(manifest);
        // Reopen both files: only a durable, readable backup authorizes mutation.
        let check = load(&folder.path().join(MANIFEST_NAME))?;
        check.verified_payload()?;
        #[cfg(windows)]
        check.verified_streams()?;
        drop(payload);
        let directory = folder.keep();
        #[cfg(unix)]
        {
            File::open(&directory)?.sync_all()?;
            File::open(directory.parent().context("备份父目录不存在")?)?.sync_all()?;
        }
        Ok(Backup { directory, record })
    })();
    // Reading can update access time. Restore it even if backup creation failed.
    metadata
        .restore(file)
        .context("无法恢复备份读取前的文件元数据")?;
    metadata.verify_restored(file)?;
    result
}

pub fn load(manifest: &Path) -> Result<Backup> {
    ensure!(
        manifest
            .file_name()
            .is_some_and(|name| name == MANIFEST_NAME),
        "请选择 manifest.json"
    );
    let record: BackupRecord = serde_json::from_reader(File::open(manifest)?)?;
    ensure!(
        record.version == 1 && record.original_path.is_absolute(),
        "备份版本或原路径无效"
    );
    Ok(Backup {
        directory: manifest.parent().context("备份目录无效")?.to_path_buf(),
        record,
    })
}

impl Backup {
    #[cfg(windows)]
    fn verified_streams(&self) -> Result<File> {
        let mut streams = File::open(self.directory.join("windows-streams.bin"))?;
        ensure!(
            digest(&mut streams)? == self.record.windows_streams_sha256,
            "Windows 数据流备份校验失败"
        );
        streams.seek(SeekFrom::Start(0))?;
        Ok(streams)
    }

    pub fn verified_payload(&self) -> Result<File> {
        let mut payload =
            File::open(self.directory.join(PAYLOAD_NAME)).context("备份原文件缺失")?;
        ensure!(
            payload.metadata()?.len() == self.record.metadata.size
                && digest(&mut payload)? == self.record.sha256,
            "备份内容校验失败，已拒绝修改或回滚"
        );
        payload.seek(SeekFrom::Start(0))?;
        Ok(payload)
    }

    /// Keep the checked payload handle open throughout modification, including recovery.
    pub fn restore_into(&self, file: &mut File, payload: &mut File) -> Result<()> {
        self.record.metadata.ensure_same_file(file)?;
        ensure!(
            payload.metadata()?.len() == self.record.metadata.size
                && digest(payload)? == self.record.sha256,
            "备份损坏，无法回滚"
        );
        #[cfg(windows)]
        self.verified_streams()?;
        write_in_place(payload, file)?;
        ensure!(digest(file)? == self.record.sha256, "回滚内容校验失败");
        #[cfg(windows)]
        let stream_check = native::streams_digest(file);
        self.record.metadata.restore(file)?;
        self.record.metadata.verify_restored(file)?;
        #[cfg(windows)]
        ensure!(
            stream_check? == self.record.windows_streams_sha256,
            "Windows 附加数据流已发生变化，不能确认完整回滚；原备份已保留"
        );
        Ok(())
    }
}

pub fn write_in_place(source: &mut File, target: &mut File) -> Result<()> {
    source.seek(SeekFrom::Start(0))?;
    target.seek(SeekFrom::Start(0))?;
    let length = std::io::copy(source, target)?;
    target.set_len(length)?;
    target.sync_all()?;
    Ok(())
}

pub fn apply_with_recovery(
    backup: &Backup,
    file: &mut File,
    payload: &mut File,
    edit: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    if let Err(error) = edit(file) {
        return match backup.restore_into(file, payload) {
            Ok(()) => Err(anyhow!(
                "修改失败，已自动恢复原内容和元数据：{error:#}；备份：{}",
                backup.directory.display()
            )),
            Err(recovery) => Err(anyhow!(
                "修改失败：{error:#}；自动恢复也失败：{recovery:#}。请保留 U 盘备份并手动回滚：{}",
                backup.directory.display()
            )),
        };
    }
    Ok(())
}

pub fn restore(manifest: &Path) -> Result<()> {
    restore_impl(manifest, validate_directory)
}

fn restore_impl(manifest: &Path, validate: impl Fn(&Path) -> Result<()>) -> Result<()> {
    let backup = load(manifest)?;
    let mut payload = backup.verified_payload()?;
    #[cfg(windows)]
    backup.verified_streams()?;
    let mut file = open_original(&backup.record.original_path)?;
    backup.record.metadata.ensure_same_file(&file)?;
    // A failed rollback must not destroy the version that existed immediately before it.
    let recovery = create_impl(
        &mut file,
        &backup.record.original_path,
        backup.directory.parent().context("备份父目录不存在")?,
        validate,
    )?;
    let mut recovery_payload = recovery.verified_payload()?;
    apply_with_recovery(&recovery, &mut file, &mut recovery_payload, |file| {
        backup.restore_into(file, &mut payload)
    })
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use windows_sys::Win32::Security::{
        GetKernelObjectSecurity, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct WindowsSnapshot {
        pub creation_100ns: i64,
        pub access_100ns: i64,
        pub write_100ns: i64,
        pub change_100ns: i64,
        pub attributes: u32,
        pub volume_serial: u32,
        pub file_index: u64,
        pub hard_links: u32,
        pub security: Vec<u8>,
    }

    fn checked(value: i32) -> Result<()> {
        if value == 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    pub fn require_removable_volume(path: &Path) -> Result<()> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut root = vec![0u16; 32768];
        unsafe {
            checked(GetVolumePathNameW(
                path.as_ptr(),
                root.as_mut_ptr(),
                root.len() as u32,
            ))?;
            ensure!(
                GetDriveTypeW(root.as_ptr()) == 2,
                "备份必须位于 Windows 识别为可移动磁盘的 U 盘，请选择 U 盘目录（不使用原磁盘备份）"
            );
        }
        Ok(())
    }

    pub fn streams_digest(file: &mut File) -> Result<String> {
        struct HashWriter(Sha256);
        impl Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut hash = HashWriter(Sha256::new());
        backup_streams(file, &mut hash)?;
        Ok(format!("{:x}", hash.0.finalize()))
    }

    pub fn backup_streams(file: &mut File, output: &mut impl Write) -> Result<()> {
        let mut context = std::ptr::null_mut();
        let result = (|| -> Result<()> {
            file.seek(SeekFrom::Start(0))?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let mut read = 0;
                // Security descriptor is stored separately in manifest.json; this preserves
                // the raw primary/alternate streams and extended attributes on FAT USB too.
                unsafe {
                    checked(BackupRead(
                        file.as_raw_handle(),
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut read,
                        0,
                        0,
                        &mut context,
                    ))?;
                }
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read as usize])?;
            }
            Ok(())
        })();
        unsafe {
            BackupRead(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                1,
                0,
                &mut context,
            );
        }
        result
    }

    pub fn file_path(file: &File) -> Result<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        let mut buffer = vec![0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        ensure!(
            length > 0 && (length as usize) < buffer.len(),
            "无法获取原文件的完整路径"
        );
        Ok(std::ffi::OsString::from_wide(&buffer[..length as usize]).into())
    }

    pub fn freeze_automatic_times(file: &File) -> Result<()> {
        use windows_sys::Win32::Foundation::FILETIME;
        let unchanged = FILETIME {
            dwLowDateTime: u32::MAX,
            dwHighDateTime: u32::MAX,
        };
        // Prevent reads/writes through this handle from changing timestamps on close.
        unsafe {
            checked(SetFileTime(
                file.as_raw_handle(),
                std::ptr::null(),
                &unchanged,
                &unchanged,
            ))
        }
    }

    pub fn ensure_regular(file: &File) -> Result<()> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        unsafe {
            checked(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
        }
        ensure!(
            info.dwFileAttributes
                & (FILE_ATTRIBUTE_REPARSE_POINT
                    | FILE_ATTRIBUTE_DIRECTORY
                    | FILE_ATTRIBUTE_ENCRYPTED
                    | FILE_ATTRIBUTE_OFFLINE)
                == 0,
            "暂不修改链接、加密或离线文件"
        );
        ensure!(
            info.nNumberOfLinks == 1,
            "文件存在多个硬链接，已跳过以免影响其他路径"
        );
        Ok(())
    }

    impl WindowsSnapshot {
        pub fn capture(file: &File) -> Result<Self> {
            let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            let mut required = 0;
            let flags =
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
            unsafe {
                checked(GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&mut basic as *mut FILE_BASIC_INFO).cast(),
                    std::mem::size_of_val(&basic) as u32,
                ))?;
                checked(GetFileInformationByHandle(file.as_raw_handle(), &mut info))?;
                GetKernelObjectSecurity(
                    file.as_raw_handle(),
                    flags,
                    std::ptr::null_mut(),
                    0,
                    &mut required,
                );
            }
            let mut security = vec![0u8; required as usize];
            if required > 0 {
                unsafe {
                    checked(GetKernelObjectSecurity(
                        file.as_raw_handle(),
                        flags,
                        security.as_mut_ptr().cast(),
                        required,
                        &mut required,
                    ))?;
                }
            } else {
                // Some filesystems (e.g. FAT) have no persistent ACLs. Other failures must block editing.
                let error = std::io::Error::last_os_error();
                ensure!(
                    matches!(error.raw_os_error(), Some(1 | 50)),
                    "无法读取原文件权限信息: {error}"
                );
            }
            Ok(Self {
                creation_100ns: basic.CreationTime,
                access_100ns: basic.LastAccessTime,
                write_100ns: basic.LastWriteTime,
                change_100ns: basic.ChangeTime,
                attributes: basic.FileAttributes,
                volume_serial: info.dwVolumeSerialNumber,
                file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                hard_links: info.nNumberOfLinks,
                security,
            })
        }

        pub fn same_identity(&self, other: &Self) -> bool {
            self.volume_serial == other.volume_serial && self.file_index == other.file_index
        }

        pub fn restore(&self, file: &File) -> Result<()> {
            let basic = FILE_BASIC_INFO {
                CreationTime: self.creation_100ns,
                LastAccessTime: self.access_100ns,
                LastWriteTime: self.write_100ns,
                ChangeTime: self.change_100ns,
                FileAttributes: self.attributes,
            };
            unsafe {
                checked(SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&basic as *const FILE_BASIC_INFO).cast(),
                    std::mem::size_of_val(&basic) as u32,
                ))
            }
        }

        pub fn set_target_times(&self, file: &File, time: SystemTime) -> Result<()> {
            use std::os::windows::fs::FileTimesExt;
            self.restore(file)?;
            file.set_times(
                FileTimes::new()
                    .set_created(time)
                    .set_accessed(time)
                    .set_modified(time),
            )?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    struct Fixture {
        source_disk: tempfile::TempDir,
        usb: tempfile::TempDir,
        path: PathBuf,
        file: File,
    }

    impl Fixture {
        fn new() -> Self {
            let source_disk = tempfile::tempdir().unwrap();
            let usb = tempfile::tempdir().unwrap();
            let path = source_disk.path().join("测试 原文件.txt");
            fs::write(&path, b"original document content, not just timestamps").unwrap();
            let file = open_original(&path).unwrap();
            let times = FileTimes::new()
                .set_accessed(UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_700))
                .set_modified(UNIX_EPOCH + Duration::new(1_600_086_400, 765_432_100));
            file.set_times(times).unwrap();
            Self {
                source_disk,
                usb,
                path,
                file,
            }
        }

        fn save(&mut self) -> Backup {
            // Only bypass removable-volume detection in this private test helper.
            // Production always validates the real Windows volume, twice.
            create_impl(&mut self.file, &self.path, self.usb.path(), |_| Ok(())).unwrap()
        }
    }

    #[test]
    fn backup_keeps_bytes_precise_metadata_and_creates_nothing_on_source_disk() {
        let mut fixture = Fixture::new();
        let before = FileSnapshot::capture(&fixture.file).unwrap();
        let saved = fixture.save();
        let loaded = load(&saved.directory.join(MANIFEST_NAME)).unwrap();
        assert_eq!(loaded.record.metadata, before);
        assert_eq!(loaded.record.sha256, digest(&mut fixture.file).unwrap());
        #[cfg(windows)]
        assert_eq!(
            loaded.record.original_path,
            native::file_path(&fixture.file).unwrap()
        );
        #[cfg(not(windows))]
        assert_eq!(
            loaded.record.original_path,
            fs::canonicalize(&fixture.path).unwrap()
        );
        assert!(saved
            .directory
            .starts_with(fs::canonicalize(fixture.usb.path()).unwrap()));
        assert_eq!(fs::read_dir(fixture.source_disk.path()).unwrap().count(), 1);
        saved.verified_payload().unwrap();
        before.restore(&fixture.file).unwrap();
        before.verify_restored(&fixture.file).unwrap();
    }

    #[test]
    fn rollback_restores_original_content_times_size_and_file_identity() {
        let mut fixture = Fixture::new();
        let saved = fixture.save();
        let mut payload = saved.verified_payload().unwrap();
        fixture.file.seek(SeekFrom::Start(0)).unwrap();
        fixture.file.write_all(b"new").unwrap();
        fixture.file.set_len(3).unwrap();
        saved
            .record
            .metadata
            .set_target_times(
                &fixture.file,
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )
            .unwrap();
        saved.restore_into(&mut fixture.file, &mut payload).unwrap();
        saved
            .record
            .metadata
            .verify_restored(&fixture.file)
            .unwrap();
        assert_eq!(digest(&mut fixture.file).unwrap(), saved.record.sha256);
    }

    #[test]
    fn interrupted_write_automatically_restores_and_retains_backup() {
        let mut fixture = Fixture::new();
        let saved = fixture.save();
        let mut payload = saved.verified_payload().unwrap();
        let error = apply_with_recovery(&saved, &mut fixture.file, &mut payload, |file| {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(b"partial")?;
            file.set_len(7)?;
            Err(anyhow!("injected disk write failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("已自动恢复"));
        saved
            .record
            .metadata
            .verify_restored(&fixture.file)
            .unwrap();
        assert_eq!(digest(&mut fixture.file).unwrap(), saved.record.sha256);
        assert!(saved.directory.join(MANIFEST_NAME).exists());
    }

    #[test]
    fn backup_destination_failure_leaves_original_unchanged() {
        let mut fixture = Fixture::new();
        let before = FileSnapshot::capture(&fixture.file).unwrap();
        let blocker = fixture.usb.path().join("not-a-directory");
        fs::write(&blocker, b"blocked").unwrap();
        assert!(create_impl(
            &mut fixture.file,
            &fixture.path,
            &blocker.join("child"),
            |_| Ok(())
        )
        .is_err());
        before.verify_restored(&fixture.file).unwrap();
        assert_eq!(fs::read_dir(fixture.source_disk.path()).unwrap().count(), 1);
        assert_eq!(fixture.file.metadata().unwrap().len(), before.size);
    }

    #[test]
    fn damaged_backup_cannot_overwrite_original() {
        let mut fixture = Fixture::new();
        let saved = fixture.save();
        fs::write(saved.directory.join(PAYLOAD_NAME), b"corrupted").unwrap();
        let before = digest(&mut fixture.file).unwrap();
        assert!(saved.verified_payload().is_err());
        let mut corrupt = File::open(saved.directory.join(PAYLOAD_NAME)).unwrap();
        assert!(saved.restore_into(&mut fixture.file, &mut corrupt).is_err());
        assert_eq!(digest(&mut fixture.file).unwrap(), before);
    }

    #[test]
    fn rollback_refuses_another_file_at_original_path() {
        let mut fixture = Fixture::new();
        let saved = fixture.save();
        drop(fixture.file);
        fs::rename(&fixture.path, fixture.path.with_extension("moved")).unwrap();
        fs::write(&fixture.path, b"unrelated file").unwrap();
        assert!(restore_impl(&saved.directory.join(MANIFEST_NAME), |_| Ok(())).is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), b"unrelated file");
    }

    #[test]
    fn repeated_backups_do_not_overwrite_history() {
        let mut fixture = Fixture::new();
        let first = fixture.save();
        let second = fixture.save();
        assert_ne!(first.directory, second.directory);
        first.verified_payload().unwrap();
        second.verified_payload().unwrap();
    }

    #[test]
    fn manual_restore_survives_closing_and_reopening_file() {
        let mut fixture = Fixture::new();
        let saved = fixture.save();
        fixture.file.seek(SeekFrom::Start(0)).unwrap();
        fixture.file.write_all(b"changed").unwrap();
        drop(fixture.file);
        restore_impl(&saved.directory.join(MANIFEST_NAME), |_| Ok(())).unwrap();
        let file = open_original(&fixture.path).unwrap();
        saved.record.metadata.verify_restored(&file).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn fixed_disk_is_rejected_as_production_backup_destination() {
        let directory = tempfile::tempdir().unwrap();
        assert!(validate_directory(directory.path()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn locked_file_is_not_edited() {
        let fixture = Fixture::new();
        assert!(open_original(&fixture.path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn alternate_stream_survives_in_place_write_and_restore() {
        let mut fixture = Fixture::new();
        drop(fixture.file);
        let stream = format!("{}:Zone.Identifier", fixture.path.display());
        fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        fixture.file = open_original(&fixture.path).unwrap();
        let saved = fixture.save();
        let mut payload = saved.verified_payload().unwrap();
        fixture.file.seek(SeekFrom::Start(0)).unwrap();
        fixture.file.write_all(b"edit").unwrap();
        saved.restore_into(&mut fixture.file, &mut payload).unwrap();
        drop(fixture.file);
        assert_eq!(fs::read(stream).unwrap(), b"[ZoneTransfer]\r\nZoneId=3\r\n");
    }
}
