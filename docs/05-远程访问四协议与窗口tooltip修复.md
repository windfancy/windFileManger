# 05 - 远程访问四协议与窗口/tooltip 修复（第六轮）

## 1. 修复：最大化后大量留白

**现象**：最大化后内容区仍停留在初始 1180×760，右侧/底部大量空白。

**根因**：Window 显式 `width/height` 把布局尺寸绑定为常量；窗口被系统最大化后
winit 只改外框，Slint 内部布局尺寸不跟随。

**修复**：`width/height` → `preferred-width/preferred-height`（1180px/760px）。
Window 的实际 width/height 属性随窗口变化，布局自动自适应。经 Slint 编译器源码
（`builtin_elements.rs` 行 2165）确认 Window 支持 preferred-width。

**副作用修复**：宽度可变后状态栏 `width: parent.width - 240px` 触发 binding loop
告警 → 改 `horizontal-stretch: 1`。

**实测**：1920×1005 最大化，工具栏/标签栏满宽、状态栏达底部 ✓。

## 2. 修复：tooltip 被面包屑/目录框遮挡

**根因**：Slint 按树序绘制。tooltip Rectangle 在 ToolBtn 内部，永远被后绘的兄弟
（面包屑栏）覆盖。

**修复**（全局浮层方案）：
- `Logic.tip-text/tip-x/tip-y` 全局属性；
- ToolBtn `changed has-hover => {}` 上报（显示时用 `self.absolute-position`）；
- Window **最后一个子元素**渲染浮层（树序最后 = 最上层），
  `x: min(max(tip-x, 4px), root.width - 264px)` 防溢出。

**自动化坑**：单点 `SetCursorPos` 瞬移不产生有效 WM_MOUSEMOVE 序列，has-hover
不触发；必须 6-8 步轨迹移动后 hover 才生效（按钮高亮 #E9EEF8 验证）。

## 3. 新增：SMB / FTP / WebDAV / DLNA 四协议远程浏览（remote.rs，~1300 行）

### 3.1 设计

- **路径即 URL**：条目 path 携带完整 URL（`\\host\share`、`ftp://`、`http(s)://`、
  `dlna://host:port/objectID`），双击目录自然继续应用内导航。
- **凭证匹配**：`host_key(url)`（`\\host` 或 `scheme://host:port`）与 `config.remotes`
  匹配；config 新增 `user/pass` 字段（`#[serde(default)]`）。
- **零新增重依赖**：仅 rustls 0.23（ring）用于 HTTPS；HTTP/FTP/SMB/XML 全手写。

### 3.2 各协议实现要点

| 协议 | 要点 |
|---|---|
| HTTP | std TcpStream 手写：Content-Length/chunked、1xx 跳过、Basic 认证 |
| HTTPS | rustls `dangerous().with_custom_certificate_verifier(NoVerify)`（NAS 自签证书全信任）；0.23 中 `dangerous_configuration` feature 已移除，API 为 `builder().dangerous()...` |
| FTP | 手写客户端：EPSV 优先 PASV 回退、CWD 进目录后 MLSD→LIST 回退、UNIX/DOS 双格式解析。**坑①**：数据连接地址用 `peer_addr().set_port(port)`——`peer_addr()` 带控制端口 21，直接连导致 10060；**坑②**：SmbFTPD 的 PASV 连接被拒（服务器 bug），EPSV 正常；**坑③**：MLSD 带路径参数被 550 拒，必须先 CWD |
| SMB | `NetShareEnumW`（level 1，过滤非磁盘/临时共享）+ `WNetAddConnection2W`（1219/81/85 视为已连接）+ UNC `read_dir`。**feature 坑**：NetShareEnum/SHARE_INFO_1 在 `Win32::Storage::FileSystem`（不是 NetManagement！），WNet 在 `Win32::NetworkManagement::WNet` |
| WebDAV | 手写 PROPFIND Depth 1 + 大小写不敏感 XML 扫描（`xml_child`/`xml_attr` 忽略命名空间前缀；`xml_attr` 的 pattern 必须用小写——先 lowercased 全文再搜） |
| DLNA | SSDP M-SEARCH（2.5s，按 IP 过滤）→ 失败回退探测 8200/49152/5000/5001/8201/8000 标准路径 → `parse_device_desc` 取 ContentDirectory controlURL → SOAP Browse + DIDL-Lite 解析；`dlna_parent` 用 BrowseMetadata 取 parentID 实现上级导航；结果缓存 `DLNA_CACHE` |

### 3.3 写操作守卫（guard_local）

SMB 走系统 FS 可写；FTP/WebDAV/DLNA 只读——delete/copy/paste/new_folder/new_file/
search/batch 等全部入口前置 `guard_local` 拦截并提示「远程目录为只读」。远程文件
双击打开暂不支持（toast 提示）。

## 4. 实测（NAS 192.168.1.16，MiniDLNA 1.3.3 + SmbFTPD + WebDAV:5006）

| 项 | 结果 |
|---|---|
| SMB | 14 共享枚举 + 共享内目录读取 ✓ |
| FTP | 根 10 项（真实 mtime）+ 子目录导航 ✓ |
| WebDAV | 10 项 + 401 无凭证错误提示 ✓ |
| DLNA | 4 根容器 + 子容器（64→2 项）+ parent 导航 + 端口回退解析 ✓ |
| UI 应用内导航 | 4 条目点击导航成功（面包屑 `192.168.1.16` / `:21` / `:5006` / `DLNA 192.168.1.16:8200`）✓ |
| 单测 | 49 通过（13 新增解析器单测；9 个 `#[ignore]` 实测需 `WFM_NAS_USER/WFM_NAS_PASS`） |
| clippy | -D warnings 零告警 ✓ |

## 5. UI 接线 bug（实测发现）

远程侧栏条目 `kind: 6` 被 app.slint `if (si.kind < 3)` 判为段头（`kind >= 3` 渲染为
纯 Text，**无 TouchArea**）——显示正常但点击无效。修复为 `kind: 0`。
教训：SideItem.kind 的取值域要收敛（0 普通 / 1 磁盘 / 2 收藏 / 3 段头），6 属于
越界值，编译期无法发现，只有实机点击测试能暴露。

## 6. 自动化经验（本轮新增）

- **PrintWindow BMP 行序**：GetDIBits 以 `biHeight=-h`（top-down）输出时数据首行
  即图像顶行；但 BMP 头若写成 `+h`（bottom-up）会误导解析（显示倒立/错位）。
  头与数据必须一致（本脚本统一 `-h` + `phys=yy` 顺序读）。
- **客户坐标换算**：客户 y = 截图物理 y − 标题栏 35（实测 ClientToScreen 确认，
  无 DPI 缩放时客户区 1180×760 = 逻辑 1:1）。
- **宿主 bash 调用间 cwd 不稳定**：脚本一律绝对路径。

## 7. 注意事项

- **凭证明文**：NAS 账号密码以明文存于 `%LOCALAPPDATA%\windFileManger\config.toml`
  （单机个人工具可接受；后续可考虑 DPAPI 加密）。
- **DLNA SSDP**：多播 M-SEARCH 在沙箱测试环境被阻断，靠端口回退探测通过；真实
  运行环境下 SSDP 发现路径应正常。
- **SMB 会话**：`WNetAddConnection2W` 建立的会话为进程级，退出即释放。
