# 360

360 是一个面向 Windows 的免安装文件时间工具。把发布目录整体复制到 U 盘后，双击 `360.exe` 即可运行，不写入安装程序注册表。

当前功能：

- 添加一个或多个文件夹，或扫描当前可见的全部磁盘；
- 默认扫描 Word、Excel、PowerPoint、PDF、TXT，可在界面中增加任意后缀；
- 按文件创建时间与最后写入时间的绝对差值筛选，支持“至少相差 X 天”和“恰好相差 X 天”；
- 修改选中文件的 Windows 创建、访问、最后写入时间；
- 对 `.docx/.docm/.xlsx/.xlsm/.pptx/.pptm` 同步修改 `docProps/core.xml` 的 `dcterms:created` 与 `dcterms:modified`；
- 修改前强制将原内容和元数据备份到 U 盘，SHA-256 校验通过才修改；
- Office 新内容先在 U 盘准备，再通过已打开的原文件句柄写回，不删除、重建原文件；
- 提供“从备份回滚”入口，恢复原内容、创建/访问/修改时间及记录的文件属性，并读回校验；
- 访问被拒绝、文件被占用、格式损坏会跳过并在错误列表中显示。

## 运行和构建方式

如果已经拿到 `360.exe`，Windows 上不需要安装 Rust、.NET 或 WebView2，直接双击即可运行。把 exe 复制到 U 盘后也可以直接双击运行。

在 Windows 机器安装 Rust 后执行：

```powershell
cargo build --release
```

发布文件构建后位于 `target\\release\\portable_file_time.exe`，打包脚本会将它命名为 `360.exe`。将该 exe 单独复制到 U 盘即可运行；Rust 程序不依赖 .NET 或 WebView2 运行时。

GitHub Actions 不是运行前提，只是两种可选用途：

- `build-windows-portable.yml`：构建并上传可复制到 U 盘的 zip；
- `test-windows.yml`：在真实 `windows-latest` 环境执行格式检查、单元测试、Windows 目标编译检查和 release 构建。

如果当前没有 Windows 开发环境，可以在 GitHub 的 **Actions → build-windows-portable → Run workflow** 手动构建，完成后从 Artifacts 下载 zip。提交代码或发起 Pull Request 时，`test-windows` 会自动验证。

Action 可以验证编译、OOXML 时间修改逻辑和 Windows 目标代码，但不能替代最终的人工实机验收，例如资源管理器显示时间、权限不足、Word 正在占用文件、U 盘读写速度等。

## 重要边界

这个工具只能调整当前文件的元数据和 Windows 文件系统时间，不能从一个已经被覆盖的 Word/Excel 文件中推导出旧正文或真正的历史版本。要恢复旧内容，仍需 Windows 文件历史、还原点、OneDrive/SharePoint、自动恢复文件或其他备份。

旧版二进制 `.doc/.xls/.ppt` 没有 OOXML 的 `core.xml`，本版本只修改其文件系统时间；若要改旧版文件内部属性，需要安装 Office 并走 Office COM 自动化，无法保持完全免安装。

## 验证建议

先用测试副本验证：修改前后分别检查资源管理器“属性 → 修改日期”和 Word/Excel 的“文件 → 信息”属性，并用重新扫描确认两套时间。若文件正在 Word/Excel 中打开，先关闭程序再修改。

## U 盘备份与回滚

默认备份位置是 `360.exe` 同级的 `360-backups` 文件夹；也可在左侧选择 U 盘目录。Windows 下必须是系统识别为可移动磁盘的卷，否则拒绝开始；某些被系统识别为固定磁盘的 USB 硬盘当前也会被拒绝。备份不可关闭，不会降级到原文件目录或系统临时目录。macOS 仅供开发验证，不执行 Windows 的可移动磁盘检查。

每个原文件、每次修改都有独立的 `backup-*` 目录：

- `original.bin`：原文件的完整主数据流，包含原有 Office 内容及内部属性。
- `manifest.json`：原绝对路径、备份时间、文件大小、SHA-256、创建/访问/修改时间；Windows 额外保存 100ns 原始时间值、元数据变更时间、文件属性、卷序列号、文件标识、硬链接数、所有者/组/DACL 安全描述符。
- `windows-streams.bin`（Windows）：通过 BackupRead 保存的主数据流、附加数据流和扩展属性的原始归档，带独立 SHA-256；安全描述符另存于 JSON。普通 U 盘文件系统也能保存这些归档字节。

备份文件和清单写入后同步到磁盘，重新读取并校验，再生成 Office 工作文件。备份或准备阶段失败不修改原内容。写回失败时尝试自动恢复；如果 U 盘被拔出、断电或进程被强制关闭，不能保证自动恢复，重新连接原 U 盘后使用回滚入口。不能把“写入过备份”视为硬件故障下的绝对保证。

点击“从备份回滚…”，选择对应 `manifest.json`，核对原路径和备份时间后确认。回滚先将当前版本再次备份到 U 盘，再恢复旧备份；回滚失败时尝试恢复回滚前版本。原文件必须仍存在于原路径且卷/文件标识一致，避免把旧备份覆盖到另一份同名文件。U 盘盘符改变时，可重新选择备份清单；原文件所在盘符改变则需恢复原盘符。

程序保持原文件身份，以保留 ACL 和附加数据流；不主动更改权限。只读、被占用、链接、多个硬链接、加密或离线文件会拒绝修改。回滚中若发现权限、身份或附加数据流被其他软件改变，会报错而非宣称完整恢复；本工具不是跨磁盘迁移或全文件系统恢复工具。Windows 审计 SACL、USN 日志、目录元数据、物理布局等不属于原样恢复承诺。不同文件系统支持的时间精度也不同，读回不一致时不得算成功。

Windows API 参考：[FILE_BASIC_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_basic_info)、[SetFileTime](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfiletime)、[BackupRead](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-backupread)。

## Windows 实机验收（尚未执行）

仅使用测试副本，在 Windows 10/11 上验证：

1. 将 EXE 放入真实 U 盘；非 U 盘备份位置应被拒绝。修改和回滚后原目录都不应产生备份或临时文件。
2. 对 TXT、DOCX/XLSX/PPTX 做修改 → 退出程序 → 重开 → 回滚。比较原内容 SHA-256、文件大小、创建/访问/写入/ChangeTime、属性、文件 ID、ACL 和 ADS，并用 Office/WPS 打开验证。
3. Word/WPS 占用、只读、无权限、U 盘满、备份损坏、备份不存在均应明确报错，未获得完整备份前不得写原内容。
4. 注入写入失败，确认自动回滚；强制结束程序或拔出 U 盘后，重新启动并从保留备份恢复。
5. 验证中文和长路径、NTFS 原盘与 FAT32/exFAT U 盘。FAT32 超过单文件大小限制时必须报错并保留原内容。
6. 原文件被其他程序替换、ACL/ADS 改动时不能假报恢复成功；反复修改与回滚不能覆盖既有备份。

本机测试及 Windows 交叉目标编译检查，不等同于上述实机验收。
