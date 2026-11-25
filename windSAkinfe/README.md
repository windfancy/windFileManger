# windSAkinfe

一个用 Rust 写的命令行文件工具：批量删除 / 复制 / 移动 / 改名、打包与解包、
Base64、哈希、查找、去重、正则替换。

**所有算法都是本项目自己实现的**——DEFLATE、LZMA/LZMA2、ZIP/7z/tar/gzip/xz/RAR 容器、
ZipCrypto、CRC32/CRC64、MD5、SHA-1/2/3、BLAKE2b/2s/3、Base64、通配匹配、
自然排序、精简正则引擎，全部手写。

依赖表里只有 `clap`（命令行参数解析），没有 `flate2` / `sha2` / `base64` / `zip` /
`lzma-rs` 这类算法库；运行时也不调用 `7z.exe`、`tar`、`rar` 等外部程序
（`7z.exe` 只在 `tests/` 里作为交叉验证的对照方出现）。

## 构建

```bash
cargo build --release
# 产物：target/release/windSAkinfe.exe（约 2.1 MB）
```

Rust 1.99+。无 C 依赖，无网络依赖。

## 命令一览

| 命令 | 别名 | 作用 |
|---|---|---|
| `rm` | `del` `remove` | 批量删除（可进回收站） |
| `cp` | `copy` | 批量复制（保留时间戳、空间检查） |
| `mv` | `move` | 批量移动（跨卷自动复制+删除） |
| `rn` | `rename` | 按 `%` 模板 + `*` 回填批量改名 |
| `touch` | | 创建文件 / 改时间戳（`-m` `-a` 分别改） |
| `link` | | 符号链接 / 硬链接 |
| `find` | | 多条件查找（名称/类型/体积/时间） |
| `stat` | | 文件详情（可附哈希） |
| `tree` | | 目录树 |
| `wc` | | 行/词/字节/字符统计（中文按字符算词） |
| `dedup` | | 重复文件查找（大小→头尾→SHA256 三级筛） |
| `pack` | `zip` `compress` | 打包：zip / 7z / tar / gz / xz，可分卷、可加密 |
| `unpack` | `unzip` `extract` | 解包：zip / 7z / tar / gz / xz / rar（只读） |
| `split` / `join` | | 按大小切分与合并 |
| `b64` | | Base64 编解码（标准 / URL-safe） |
| `hash` | | 15 种哈希，可生成/校验清单 |
| `replace` | | 正则或字面量替换 |
| `cfg` | | 配置查看与修改 |

全局选项：`--json`、`--no-color`、`-q`、`-v`、`-y`、`-n`（预演）、`-j`（并行数）。
选择类选项在所有文件命令里通用：`-p/--pattern`、`--include`、`--exclude`、
`--files-only`、`--dirs-only`、`--depth`、`--case-sensitive`。
冲突类：`--on-conflict skip|overwrite|rename`、`-f/--force`。

`rm` 和 `cp` 默认就是递归处理目录的，`-r` 只为兼容习惯写法，加不加一样。

## 常用示例

### 文件操作

```bash
# 预演：看清楚要做什么，不动磁盘
windSAkinfe rm -n 'build/*.o'

# 删除，先确认（超过 200 项或 1 GiB 会要求确认）
windSAkinfe rm 'build/**' --keep-readonly
windSAkinfe rm --trash old.txt          # 进系统回收站

# 复制并保留时间戳（默认），额外保留权限位
windSAkinfe cp --preserve-perms src/ dst/
windSAkinfe cp --no-preserve big.iso /mnt/bak/     # 不保留 mtime

# 只复制 *.jpg，拍平到同一层
windSAkinfe cp photos/ out/ -p '*.jpg' --flat

# 移动多个文件到一个目录，重名自动加序号
windSAkinfe mv a.txt b.txt c.txt archive/ --on-conflict rename

# 批量改名：`%3d` 补零序号，`*` 回填原名的捕获部分
windSAkinfe rn 'IMG_*.jpg' -t 'photo_%3d.jpg'
windSAkinfe rn photos/ -t 'photo_%3d.jpg'                  # 目录会自动展开成里面的文件
windSAkinfe rn photos/ -t '%{Y}-%{m}-%{d}_%3d.jpg'          # 用文件的修改日期
windSAkinfe rn photos/ -t '%3d.jpg' --number-mode per-dir   # 每个目录重新计数

# 时间戳：只改访问时间
windSAkinfe touch -a -t '2024-01-01 00:00:00' *.log

# 硬链接（Windows 上符号链接需要开发者模式或管理员权限）
windSAkinfe link --hard big.bin big_link.bin
```

改名模板的占位符：

| 写法 | 含义 |
|---|---|
| `%d` / `%3d` | 序号（`%3d` 补零到 3 位） |
| `%c` / `%3c` | 字母序号（a, b, …, z, aa） |
| `*` | 回填名称模式里 `*` 捕获到的部分 |
| `%f` `%e` | 原文件名（不含扩展名）、原扩展名 |
| `%p` `%P` | 父目录名、相对根目录的路径 |
| `%i` `%s` | 字节数 |
| `%m` `%H` `%M` `%S` | mtime 的 `YYYYMMDD` / 时 / 分 / 秒 |
| `%{Y}` `%{m}` `%{d}` | mtime 的年 / 月 / 日（**注意 `%d` 是序号，日期字段必须带花括号**） |
| `%{H}` `%{M}` `%{S}` | mtime 的时 / 分 / 秒 |
| `%{D}` `%{T}` | mtime 的 `YYYYMMDD` / `HHmmss` |
| `%{now:Y}` … | 同上，但取**当前时间**而不是文件 mtime |
| `%u` | 随机串 |
| `%%` | 字面 `%` |
| `\*` | 字面 `*` |

### 查找与统计

```bash
# 大于 100M 且最近 7 天修改过的文件
windSAkinfe find . --type file --min-size 100M --newer 7d

# 空文件 / 空目录
windSAkinfe find . --type empty

# 目录树，按体积排序，只到 2 层
windSAkinfe tree . --depth 2 --sort size

# 中文文本的行/词/字节/字符统计（词按 Unicode 字符判空白，中文不会被拆开）
windSAkinfe wc src/a.rs --chars --total

# 找重复照片并给出可回收空间（默认只报告）
windSAkinfe dedup photos/ --min-size 1M
windSAkinfe dedup photos/ --delete keep-first      # 每组保留第一个
windSAkinfe dedup photos/ --hardlink              # 用硬链接省空间
```

### 打包 / 解包

```bash
# 打包（格式按扩展名推断，也可 -f 强制）
windSAkinfe pack src/ docs/ -o backup.zip -l 9
windSAkinfe pack src/ -o backup.7z -l 6
windSAkinfe pack src/ -o backup.tar
windSAkinfe pack big.log -o big.log.gz
windSAkinfe pack data/ -o data.tar.xz

# 排除 + 只收某类文件
windSAkinfe pack proj/ -o p.zip --exclude '*.o' --exclude 'build/*'
windSAkinfe pack proj/ -o src.zip --include '*.rs' --include '*.toml'

# 分卷：100M 一卷
windSAkinfe pack big/ -o big.zip --split 100M

# 加密（仅 zip，ZipCrypto）。注意 `-p` 是「名称模式」，密码用 --password
windSAkinfe pack secret/ -o s.zip --password 'hunter2'
windSAkinfe unpack s.zip -o out/ --password 'hunter2'

# 先看内容
windSAkinfe unpack a.zip -l
windSAkinfe unpack a.zip -l --include '*.rs'

# 解包：只取部分、拍平、冲突重命名
windSAkinfe unpack a.zip -o out/ --include 'docs/*'
windSAkinfe unpack a.zip -o out/ --flat
windSAkinfe unpack a.zip -o out/ --on-conflict overwrite

# 分卷与合并
windSAkinfe split big.bin --size 100M     # 生成 big.bin.001 ...
windSAkinfe join big.bin.001 -o restored.bin
```

### 编码与哈希

```bash
# Base64（不加 -d 就是编码；`-p` 是模式，所以这里没有 enc/dec 子命令）
windSAkinfe b64 file.bin -o file.b64
windSAkinfe b64 -d file.b64 -o file.bin
printf 'hi' | windSAkinfe b64 -                    # 管道，`-` 表示 stdin
windSAkinfe b64 -u --no-pad file.bin               # URL-safe，不带填充
windSAkinfe b64 --wrap 76 file.bin -o file.b64     # 每行 76 字符

# 哈希：15 种算法
windSAkinfe hash file.iso                          # 默认 sha256
windSAkinfe hash -a sha256 -a blake3 -a md5 file.iso
windSAkinfe hash -a sha256 --upper --base64 file.iso
windSAkinfe hash -a sha256 --write manifest.txt file.iso dir/   # 生成 sha256sum 兼容清单
windSAkinfe hash -a sha256 -c manifest.txt                       # 校验（OK / FAILED / MISSING）
windSAkinfe hash -a sha256 --recursive photos/                    # 递归处理目录

# 正则替换（支持 $1 $2 $0）
windSAkinfe replace 'src/*.rs' -p 'fn\s+(\w+)' -r 'pub fn $1'
windSAkinfe replace notes/ -p 'foo' -r 'bar' --literal --write --backup .bak
windSAkinfe replace log.txt -p 'ERROR' -r 'ERR' -i        # 忽略大小写
```

### 结构化输出

```bash
windSAkinfe find . --type file --json | jq -r '.[].path'
windSAkinfe --json stat big.bin
```

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 全部成功 |
| 1 | 有单项失败（权限、占用、IO 失败等） |
| 2 | 用法错误（参数不合法、目标不存在、条件不成立） |
| 3 | 资源不足（剩余空间不够） |
| 4 | 安全护栏拦截 |
| 5 | 不支持（RAR 私有方法、7z AES、加密压缩包缺密码等） |

## 安全护栏

- 拒绝删除根目录、盘符根、用户主目录、系统目录、当前工作目录。
- 拒绝 `mv dir dir/sub` 这类「搬到自己内部」的操作。
- 解包时做 Zip Slip 防护：任何解出的路径必须落在目标目录内。
- 超过 200 项或 1 GiB 的破坏性操作会要求确认（`-y` 跳过，`-n` 只预演）。
- `--dry-run` 与真实执行走**同一条**计划生成路径，预演结果即真实行为。

## 不支持的东西

- 创建 RAR（只能读，且只支持 store 方法）。
- RAR 私有压缩方法、7z 的 AES 加密、ZIP 的 AES 加密（只用 ZipCrypto）。
- 符号链接在 Windows 上需要开发者模式或管理员权限，否则报 `unsupported`
  （可用 `link --hard` 替代）。

## 质量

```bash
cargo test              # 132 个单元测试 + 7 个与 7-Zip 的互操作测试
cargo clippy --all-targets   # 零告警
```

互操作测试会真的调用 `C:\Program Files\7-Zip\7z.exe`，双向验证：
我们产的 zip/7z/加密 zip 能被 7-Zip 正常读取（`7z t` 全通过），
7-Zip 产的 zip/7z/加密 zip 我们也能正确解开。

哈希实现与 Python `hashlib`、`crc32` 逐位比对；Base64 与标准实现往返比对；
自动生成的 xz / gzip 用系统 `xz`、`gzip` 交叉验证。

性能实测见 [`docs/03-性能基准.md`](docs/03-性能基准.md)（SHA256 920 MB/s、
10 万文件遍历 0.41 s 等）。

## 目录结构

```
src/
  cli/        clap 定义（仅参数解析，不含业务逻辑）
  cmd/        各命令的业务实现（rm/cp/mv/rn/pack/unpack/hash/...）
  plan/       计划与执行分离、冲突策略、护栏、Summary/退出码
  fsx/        文件系统原语（遍历、复制、删除、时间戳、回收站）
  archive/    zip / 7z / tar / gz / xz / rar 容器读写
  codec/      DEFLATE、LZMA/LZMA2、位流
  algo/       哈希、Blake、Base64、通配、自然排序、模板、精简正则
  term/       颜色、进度、JSON、确认
  util/       字节读写、变长整数、时间格式
docs/
  01-需求说明书.md   功能清单、红线、退出码、验收标准
  02-架构设计.md     分层、数据结构、关键决策
  03-性能基准.md     六项指标的实测数据与复现方法
tests/
  interop.rs         与 7-Zip 的双向互操作测试
```
