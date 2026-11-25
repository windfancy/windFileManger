# 07 — 管理员模式 · 快捷键 · Tab 级搜索 · 批量压缩解压 · 设置页面

> 第十四轮功能规格。对应五项用户需求，全部基于既有代码结构（见 01/02/03/05/06 号规格）。

## 1. 管理员模式强制（运行前提示）

### 背景与问题
MFT 直读需要 `\\.\X:` 的 GENERIC_READ（管理员权限）。非管理员运行时全部盘走文件夹枚举降级，索引不全。用户要求：**启动时检测，非管理员先提示，可提权重启**。

### 行为
1. `main()` 在创建 Slint 窗口**之前**检测管理员身份（`IsUserAnAdmin`，shell32）。
2. 非管理员 → 原生 `MessageBoxW`（MB_ICONQUESTION, MB_YESNO）：
   > windFileManger 需要管理员权限才能直读 MFT 建立完整全盘索引（否则搜索范围不完整）。
   > 是否以管理员身份重新启动？
   - **是** → `ShellExecuteW("runas")` 重启自身（当前 exe 路径），当前进程 `exit(0)`。若用户在 UAC 取消 → 保留当前进程继续普通模式。
   - **否** → 继续普通模式（文件夹枚举降级，秒搜范围受限）。
3. 提示开关 `config.admin_prompt`（默认 `true`）。在设置页可关闭；关闭后启动不再询问。
   - 仅当「这次会话没有提权成功且用户点了否」时才有关闭的意义；提权成功的管理员会话永远不弹。

### 技术点
- `windows-sys` 特性：`Win32_Security`（Authorization）、`Win32_UI_Shell`（ShellExecuteW）、`Win32_UI_WindowsAndMessaging`（MessageBoxW）。
- 提权重启用 `ShellExecuteW(hwnd=null, "runas", exe_path, 参数=null, cwd, SW_SHOWNORMAL)`；成功后 `std::process::exit(0)`。
- 必须在 `force_resizable` 等窗口逻辑之前决策，避免弹窗时序问题。

## 2. 文件操作快捷键

### 默认键位（全部可配置）
| 动作 | 默认键 | 说明 |
|---|---|---|
| 删除到回收站 | Delete | 对当前选中（含多选/搜索视图） |
| 重命名 | F2 | 单条（多条选中时取第一条） |
| 复制 | Ctrl+C | |
| 剪切 | Ctrl+X | |
| 粘贴 | Ctrl+V | |
| 全选 | Ctrl+A | 自动进入多选模式并全选 |
| 后退 | Alt+Left | |
| 前进 | Alt+Right | |
| 上级目录 | Backspace | |
| 刷新 | F5 | |
| 多选模式 | Ctrl+M | 进入/退出 |
| 聚焦搜索框 | Ctrl+F | |
| 退出/取消 | Escape | 多选模式→退出；搜索视图→关闭搜索；对话框→取消 |

### 实现
- `app.slint` 根级 `FocusScope`（`key-handler(e)`）：组合 `e.modifiers` 与 `e.text` 为规范键名串（`"Delete"`、`"Ctrl+C"`、`"Alt+Left"`、`"F5"`…）→ `Logic.hotkey(键名)`。LineEdit 获得焦点期间快捷键不触发（正常文本输入优先）。
- Rust 侧 `on_hotkey`：查 `cfg.hotkeys`（`Vec<(String 动作名, String 键名)>`）反查动作名分派。内置动作名与键位序列化进 `config.toml`；用户改键只改值。
- Escape 属保留键（不写配置表，固定行为）。
- **键名规范**：修饰键序 `Ctrl+Alt+Shift+主键`；主键取 `e.text` 原文（单字符）或具名键（`F1..F12`、`Delete`、`Backspace`、`Up/Down/Left/Right`、`Home/End`）。Rust 侧提供 `normalize_key(text, ctrl, alt, shift)` 纯函数，单测覆盖。

### 设置页配置
设置页列出动作名+当前键位；点击某行 → 进入「按下新键」捕获态（下一击写入；Esc 取消）。冲突检测：新键位已被其他动作占用 → 提示并保留旧值。

## 3. Tab 级搜索（搜索框与选择路径绑定）

### 状态迁移
`Tab` 增加字段：
```rust
struct Tab {
    // 既有 ...
    search_query: String,   // 搜索框文本
    search_ext: i32,        // 后缀筛选下拉
    search_active: bool,    // 搜索视图激活
    search_hits: Vec<EntryData>,  // 结果快照
    search_root: String,    // 发起搜索时的目录
}
```
- `AppState.search_active` 删除，一律由 `active_tab()` 派生。
- **切 tab**：把当前 search_model 行快照写回旧 tab，再把目标 tab 的快照+query+ext 恢复到 UI（search_model.set_vec、search-active、searching 标志）。
- **导航（navigate/list_current）**：清空当前 tab 的搜索态（结果过期）。
- `search-ext-filter` ComboBox 随 tab 切换恢复。

### 路径限定
- OwnIndex（秒搜）：`index_query` 结果在 UI 线程过滤 `Path::new(&h.path).starts_with(root_dir)`；命中数与耗时日志仍打全量。
- Legacy 递归：`start_search` 本来就以 `root_dir` 为根，天然满足。
- 面包屑黄条显示 `搜索结果（<tab 当前路径>）`。

### UI
工具栏搜索框 `text <=> Logic.search-input`；tab 切换时 Rust 回填 `search-input`。取消按钮行为不变（清当前 tab 搜索态）。

## 4. 多选批量压缩 / 解压

### 压缩
既有 action 13/14 已支持多选（`selected_paths()`）——补一个批量默认名规则确认，无代码改动。

### 批量解压（新增 action 21）
- 触发：多选模式下右键菜单新增「批量解压到各自目录」；单选保持原「解压到当前目录」（action 15）。
- 行为：对 `selected_paths()` 过滤 `is_archive` → 逐个 `extract_smart(p, 父目录)`；汇总结果 toast：
  > 批量解压：N 成功 / M 跳过（非归档）/ K 失败（首个错误）
- 全部在后台线程执行，busy 状态包裹。

### rar 排除
- 现状：windSAkinfe `Kind::Rar` 只解析条目清单，`read()` 不支持（无法解出内容），但 `sakinfe::is_archive` 返回 true → 误导。
- 修改：`sakinfe::is_archive` 排除 `Kind::Rar`；解压入口检测 `.rar` 明确提示「不支持 RAR 格式」。
- 右键菜单文案「解压到当前目录（zip/7z/tar/gz/xz）」同步更新。
- 单测：`is_archive` 对 `.rar` 返回 false，`.zip/.7z/.tar.gz/.tgz/.xz/.gz/.tar` 返回 true。

## 5. 设置页面（dialog-mode = 8）

### 内容与布局（420×340 对话框）
1. **UI 色调模式**：ComboBox（浅色 / 深色 / 跟随系统），即改即生效。
2. **快捷键**：ListView 展示 `HotkeyRow { action, key }`；每行「修改」按钮 → 捕获态；冲突提示红字。
3. **搜索后缀筛选类型**：ListView 展示 `ExtRow { label, exts }`（内置默认：全部/txt/md/pdf/图片/视频）；支持添加（label + 分号分隔扩展名）与删除。序号与搜索框下拉 model 对应。
4. **管理员提示开关**：checkbox「启动时提示以管理员身份运行」。

### Theme global（色调系统）
`app.slint` 定义 `export global Theme`，语义色集中管理：
```
bg（窗口底） panel（工具栏/状态栏） side-bg list-hover list-selected
fg fg-secondary fg-muted border card-bg accent accent-hover checkbox-border
search-bar-bg（黄条） toast-ok toast-ok-border toast-err toast-err-border ...
```
全部硬编码颜色替换为 `Theme.xxx` 引用；`Theme.mode`（0浅 1深 2跟随系统）由 Rust 设置；跟随系统读取 `Palette.color-scheme`（std-widgets）。深色配色（egui 风格）：
- bg #1E1F24 / panel #23252B / side-bg #202127 / fg #E8EAED / 次级 #BDC1C6 / border #3A3F47 / accent 保持 #1A73E8（可辨识）。

### 配置持久化（config.toml 新字段）
```toml
admin_prompt = true        # 启动管理员提示
theme = 0                  # 0浅 1深 2跟随系统
hotkeys = { delete = "Delete", rename = "F2", ... }   # 动作名 → 键名
ext_filters = [{ label = "图片", exts = "jpg;jpeg;png;gif;webp;bmp;svg" }, ...]
```
`ext_filters` 默认六项（含「全部」）；用户增删后搜索框下拉 model 动态重建（`Logic.ext-filter-model` in-out `[string]`）。

## 6. 验收清单
- [ ] 非管理员启动弹提示；提权重启成功；关闭提示开关后不再弹
- [ ] Delete/F2/Ctrl+C/X/V/A/F5/Backspace/Alt+←/→ 在普通视图生效；LineEdit 聚焦时不误触发
- [ ] Ctrl+F 聚焦搜索框；Escape 分层退出
- [ ] 每个 tab 独立搜索框文本/筛选/结果；秒搜结果限定 tab 当前路径子树
- [ ] 多选 ≥2 压缩出 zip/tar.gz；批量解压汇总成功/失败；.rar 明确提示不支持
- [ ] 深色模式全界面无白底残留；设置即时生效且重启保持
- [ ] 设置页改键冲突提示；后缀筛选增删即时反映到工具栏下拉
- [ ] 全部单测 + clippy 干净
