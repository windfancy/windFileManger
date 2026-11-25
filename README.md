# windFileManger

一个基于 **Rust + Slint** 的轻量文件管理器，复刻 NanoFile 的核心体验，但采用**平铺式导航**（侧栏平铺 + 面包屑 + 当前目录平面列表），不使用树形目录。

仅支持 Windows。

## 特性

- **浏览**：多标签页、列表/宫格视图、面包屑导航、前进/后退/上级、显示隐藏文件、类型分类着色、目录大小后台统计。
- **文件操作**：新建、重命名、删除到回收站、复制/剪切/粘贴、系统默认打开、在资源管理器中显示，与 Explorer 通过剪贴板互通。
- **快捷键**：Delete / F2 / Ctrl+C/X/V/A、Alt+Left/Right、Backspace、F5、Ctrl+M、Ctrl+F、Esc，键位可在设置页改绑。
- **搜索**：
  - 当前目录递归搜索；
  - Everything 式全盘秒搜：自建索引（NTFS 卷直读 `$MFT`，需管理员；其他情况回退目录枚举），支持 `AND` / `OR` / `NOT`、通配符与 `ext:` / `size:` / `dm:` / `path:` / `file:` / `folder:` 过滤语法。索引落盘在 exe 同级目录，约 350 万条目、154 MB、加载 1.4 秒。
- **远程浏览**：SMB / FTP / WebDAV / DLNA 四种协议，路径即 URL，凭证按 host 匹配；SMB 可写，其余三种只读。
- **批量处理**：批量重命名（模板引擎 + 预览）、批量哈希，来自内置的 windSAkinfe。
- **预览**：图片直接显示，文本前 64 KB 预览，其他类型显示元信息。
- **配置持久化**：窗口尺寸/最大化状态、标签页与当前路径、视图、排序、收藏夹、快捷键等写入 `%LOCALAPPDATA%\windFileManger\config.toml`。

项目坚持**独立实现**原则：压缩/哈希/索引/远程协议均自行实现，不调用 Everything 或其他第三方程序。

## 技术栈

- Rust 2024 edition
- Slint 1.13.1（UI）
- serde / toml / chrono
- windows-sys（Win32 API）
- rustls（WebDAV / DLNA 的 HTTPS，ring 后端）
- windsakinfe（仓库内 `windSAkinfe/` 副本，路径依赖）

## 构建与运行

需要 Rust 稳定版工具链和 Windows SDK 环境。

本项目的压缩/哈希能力来自仓库内自带的 `windSAkinfe/` 源码副本，以路径依赖方式引用（`path = "windSAkinfe"`）。clone 后即可构建，不需要再单独取得外部工程：

```
windFileManger/
├── Cargo.toml
├── windSAkinfe/          # 内置源码副本（压缩/哈希/编码算法）
└── ...
```

```powershell
cd D:\github\windFileManger
cargo build --release
cargo test
cargo clippy -- -D warnings
```

运行：

```powershell
.\target\release\windFileManger.exe
# 带初始目录启动
.\target\release\windFileManger.exe D:\code
```

启动时会提示是否以管理员身份重启：管理员权限下才能直读 NTFS `$MFT` 建立完整全盘索引；否则自动降级为目录枚举，搜索范围受限。可在设置页关闭该提示。

## 目录结构

```
windFileManger/
├── Cargo.toml
├── build.rs                 # slint-build + winresource(图标)
├── ui/app.slint             # 全部 UI
├── src/
│   ├── main.rs              # 事件循环、全局状态、后台线程调度
│   ├── config.rs            # 配置读写与会话持久化
│   ├── fs.rs                # 快速目录列举、类型分类
│   ├── ops.rs               # 文件操作、回收站、剪贴板
│   ├── remote.rs            # SMB / FTP / WebDAV / DLNA
│   ├── instant.rs           # 全盘索引调度与搜索
│   ├── mft.rs               # NTFS $MFT 直读解析
│   ├── sakinfe.rs           #  windsakinfe 批量处理适配
│   ├── preview.rs           # 图片/文本预览
│   └── tasks.rs             # 后台任务与取消
├── tests/                   # 测试
├── docs/                    # 功能规格与迭代记录
├── windSAkinfe/             # 内置源码副本（路径依赖）
└── assets/                  # 图标
```

## 文档

`docs/` 下按编号记录功能规格与实现细节：

- `01-功能规格.md`：总体功能与复刻范围
- `02-批量处理功能规格.md`：批量重命名/哈希
- `03-会话状态持久化规格.md`
- `04-功能补充说明.md`
- `05-远程访问四协议与窗口tooltip修复.md`
- `06-秒搜功能规格.md`
- `07-管理员模式快捷键Tab搜索批量压缩与设置页规格.md`
- `08-增量索引更新规格.md`

## 协议

MIT License，详见 [LICENSE](LICENSE)。可以自由使用、修改、分发和商用，只需保留版权与许可声明；不强制开源衍生作品，也不提供任何担保。
