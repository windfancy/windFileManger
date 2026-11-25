//! 命令行定义（clap derive）。只做参数解析与校验，不含业务逻辑。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "windSAkinfe",
    version,
    about = "windSAkinfe —— 批量文件操作瑞士军刀（压缩/哈希/编码算法全部自研）",
    long_about = "windSAkinfe 提供批量删除、复制、移动、改名、打包解包、分卷合并、\
Base64、哈希、查找、去重、文本替换等日常文件操作。\n\
所有压缩、哈希、编码、通配匹配算法均为本项目自研实现，不依赖任何第三方算法库，\
也不调用外部程序。",
    disable_help_subcommand = true,
    propagate_version = true
)]
pub struct Cli {
    /// 以 JSON 输出结果（stdout 只出 JSON）
    #[arg(long, global = true)]
    pub json: bool,

    /// 关闭彩色输出
    #[arg(long = "no-color", global = true)]
    pub no_color: bool,

    /// 静默模式，只输出错误
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// 输出更多细节
    #[arg(short = 'v', long, global = true)]
    pub verbose: bool,

    /// 所有确认自动回答 yes
    #[arg(short = 'y', long = "yes", global = true)]
    pub yes: bool,

    /// 预演：只显示将要做什么，不动磁盘
    #[arg(short = 'n', long = "dry-run", global = true)]
    pub dry_run: bool,

    /// 并行线程数（默认 CPU 核心数）
    #[arg(short = 'j', long, global = true)]
    pub jobs: Option<usize>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 批量删除文件与目录
    #[command(visible_alias = "del", visible_alias = "remove")]
    Rm(RmArgs),

    /// 批量复制
    #[command(visible_alias = "copy")]
    Cp(CpArgs),

    /// 批量移动
    #[command(visible_alias = "move")]
    Mv(MvArgs),

    /// 按规律批量改名
    #[command(visible_alias = "rename")]
    Rn(RnArgs),

    /// 修改文件时间戳
    Touch(TouchArgs),

    /// 创建符号链接 / 硬链接
    Link(LinkArgs),

    /// 按条件查找文件
    Find(FindArgs),

    /// 显示文件/目录详细信息
    Stat(StatArgs),

    /// 打印目录树
    Tree(TreeArgs),

    /// 统计行数/词数/字节数
    Wc(WcArgs),

    /// 查找重复文件
    Dedup(DedupArgs),

    /// 打包压缩
    #[command(visible_alias = "zip", visible_alias = "compress")]
    Pack(PackArgs),

    /// 解压
    #[command(visible_alias = "unzip", visible_alias = "extract")]
    Unpack(UnpackArgs),

    /// 按大小切分文件
    Split(SplitArgs),

    /// 合并分卷文件
    Join(JoinArgs),

    /// Base64 编码/解码
    B64(B64Args),

    /// 计算/校验哈希
    Hash(HashArgs),

    /// 文本正则替换
    Replace(ReplaceArgs),

    /// 查看与修改配置
    Cfg(CfgArgs),
}

// ---------------------------------------------------------------- 通用参数组

#[derive(Args, Debug, Clone)]
pub struct SelectArgs {
    /// 按名称模式过滤，可重复（支持 `*` `?` `[]` `{}` `**`）
    #[arg(short = 'p', long = "pattern")]
    pub pattern: Vec<String>,

    /// 只处理文件
    #[arg(long = "files-only", conflicts_with = "dirs_only")]
    pub files_only: bool,

    /// 只处理目录
    #[arg(long = "dirs-only")]
    pub dirs_only: bool,

    /// 最大递归深度（0 = 只处理直接子项）
    #[arg(long)]
    pub depth: Option<usize>,

    /// 包含路径模式
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// 排除路径模式
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,

    /// 模式大小写敏感（默认 Windows 不敏感、其它平台敏感）
    #[arg(long = "case-sensitive")]
    pub case_sensitive: bool,

    /// 小写别名：--cs
    #[arg(long = "cs", hide = true)]
    pub cs_alias: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ConflictArgs {
    /// 冲突策略
    #[arg(long = "on-conflict", value_name = "skip|overwrite|rename", default_value = "skip")]
    pub on_conflict: String,

    /// 覆盖已存在目标（等价 --on-conflict overwrite）
    #[arg(short = 'f', long)]
    pub force: bool,
}

// ---------------------------------------------------------------- rm

#[derive(Args, Debug)]
pub struct RmArgs {
    /// 要删除的路径，支持通配符
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 只删空目录（等价于只删顶层，不递归）
    #[arg(long = "no-recursive")]
    pub no_recursive: bool,

    /// 递归（本工具默认就递归，此参数只为兼容 `rm -r` 的习惯写法）
    #[arg(short = 'r', long = "recursive")]
    pub recursive: bool,

    #[command(flatten)]
    pub select: SelectArgs,

    /// 送入系统回收站（Windows 回收站 / Linux Trash 目录）
    #[arg(long)]
    pub trash: bool,

    /// 保留只读属性（默认遇到只读文件会清位重试）
    #[arg(long = "keep-readonly")]
    pub keep_readonly: bool,
}

// ---------------------------------------------------------------- cp

#[derive(Args, Debug)]
pub struct CpArgs {
    /// 源路径，可多个
    #[arg(required = true, value_name = "SRC")]
    pub src: Vec<PathBuf>,

    /// 目标路径（最后一个参数）
    #[arg(value_name = "DST")]
    pub dst: PathBuf,

    /// 拍平：把所有文件放到目标目录同一层
    #[arg(long)]
    pub flat: bool,

    /// 递归（目录默认就递归，此参数只为兼容 `cp -r` 的习惯写法）
    #[arg(short = 'r', long = "recursive")]
    pub recursive: bool,

    /// 额外保留权限位（mtime 总是保留）
    #[arg(long = "preserve-perms")]
    pub preserve_perms: bool,

    /// 不保留时间戳（默认保留 mtime/atime）
    #[arg(long = "no-preserve")]
    pub no_preserve: bool,

    /// 复制符号链接本身而非目标内容
    #[arg(long = "copy-links")]
    pub copy_links: bool,

    /// 跳过剩余空间检查
    #[arg(long = "no-space-check")]
    pub no_space_check: bool,

    #[command(flatten)]
    pub select: SelectArgs,

    #[command(flatten)]
    pub conflict: ConflictArgs,
}

// ---------------------------------------------------------------- mv

#[derive(Args, Debug)]
pub struct MvArgs {
    /// 源路径，可多个
    #[arg(required = true, value_name = "SRC")]
    pub src: Vec<PathBuf>,

    /// 目标路径（最后一个参数）
    #[arg(value_name = "DST")]
    pub dst: PathBuf,

    /// 拍平
    #[arg(long)]
    pub flat: bool,

    #[command(flatten)]
    pub select: SelectArgs,

    #[command(flatten)]
    pub conflict: ConflictArgs,
}

// ---------------------------------------------------------------- rn

#[derive(Args, Debug)]
pub struct RnArgs {
    /// 要改名的路径，支持通配符
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 命名模板，例如 `%3d.jpg`、`*_%c%s`
    #[arg(short = 't', long, value_name = "TEMPLATE")]
    pub template: String,

    /// 序号起始值
    #[arg(long = "start", default_value_t = 1)]
    pub start: u64,

    /// 序号步长
    #[arg(long = "step", default_value_t = 1)]
    pub step: u64,

    /// 序号模式：seq（全局连续）或 per-dir（每个目录重新计数）
    #[arg(long = "number-mode", value_name = "seq|per-dir", default_value = "seq")]
    pub number_mode: String,

    /// 目录也参与改名
    #[arg(long = "include-dirs")]
    pub include_dirs: bool,

    #[command(flatten)]
    pub select: SelectArgs,

    #[command(flatten)]
    pub conflict: ConflictArgs,
}

// ---------------------------------------------------------------- touch

#[derive(Args, Debug)]
pub struct TouchArgs {
    /// 目标路径，支持通配符
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 不存在则不创建
    #[arg(short = 'c', long = "no-create")]
    pub no_create: bool,

    /// 指定时间（YYYYMMDDHHMM[.SS] 或 "YYYY-MM-DD HH:MM:SS"）
    #[arg(short = 't', long)]
    pub time: Option<String>,

    /// 只改修改时间
    #[arg(short = 'm', long)]
    pub mtime_only: bool,

    /// 只改访问时间
    #[arg(short = 'a', long)]
    pub atime_only: bool,

    #[command(flatten)]
    pub select: SelectArgs,
}

// ---------------------------------------------------------------- link

#[derive(Args, Debug)]
pub struct LinkArgs {
    /// 链接指向的目标
    #[arg(value_name = "TARGET")]
    pub target: PathBuf,

    /// 链接文件路径
    #[arg(value_name = "LINK")]
    pub link: PathBuf,

    /// 创建硬链接
    #[arg(long, conflicts_with = "symbolic")]
    pub hard: bool,

    /// 创建符号链接（默认）
    #[arg(long)]
    pub symbolic: bool,

    /// 目标已存在时先删除再重建
    #[arg(short = 'f', long)]
    pub force: bool,
}

// ---------------------------------------------------------------- find

#[derive(Args, Debug)]
pub struct FindArgs {
    /// 查找根目录，可多个（默认当前目录）
    #[arg(value_name = "ROOT")]
    pub roots: Vec<PathBuf>,

    /// 名称模式
    #[arg(short = 'p', long = "pattern")]
    pub pattern: Vec<String>,

    /// 类型过滤
    #[arg(short = 't', long = "type", value_name = "file|dir|link|empty")]
    pub kind: Option<String>,

    /// 最小体积
    #[arg(long = "min-size")]
    pub min_size: Option<String>,

    /// 最大体积
    #[arg(long = "max-size")]
    pub max_size: Option<String>,

    /// 新于（7d / 24h / 2024-01-01）
    #[arg(long)]
    pub newer: Option<String>,

    /// 旧于
    #[arg(long)]
    pub older: Option<String>,

    /// 最大深度
    #[arg(long)]
    pub depth: Option<usize>,

    /// 包含路径模式
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// 排除路径模式
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,

    /// 长格式输出（体积 + 时间）
    #[arg(long)]
    pub long: bool,

    /// 结果按体积排序
    #[arg(long = "sort-size")]
    pub sort_size: bool,
}

// ---------------------------------------------------------------- stat

#[derive(Args, Debug)]
pub struct StatArgs {
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 追加哈希值
    #[arg(short = 'a', long = "hash", value_name = "ALGO", default_missing_value = "sha256", num_args = 0..=1)]
    pub hash: Option<String>,

    /// 递归统计目录
    #[arg(long)]
    pub recursive: bool,
}

// ---------------------------------------------------------------- tree

#[derive(Args, Debug)]
pub struct TreeArgs {
    /// 根目录
    #[arg(default_value = ".", value_name = "ROOT")]
    pub root: PathBuf,

    /// 最大深度
    #[arg(long, default_value_t = 3)]
    pub depth: usize,

    /// 只显示目录
    #[arg(long = "dirs-only")]
    pub dirs_only: bool,

    /// 排序方式
    #[arg(long, value_name = "name|size", default_value = "name")]
    pub sort: String,

    /// 不统计体积（大目录快很多）
    #[arg(long = "no-size")]
    pub no_size: bool,

    /// 排除模式
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
}

// ---------------------------------------------------------------- wc

#[derive(Args, Debug)]
pub struct WcArgs {
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 额外输出字符数（UTF-8 码点）
    #[arg(long)]
    pub chars: bool,

    /// 追加合计行
    #[arg(long)]
    pub total: bool,

    /// 二进制文件也统计
    #[arg(long)]
    pub binary: bool,
}

// ---------------------------------------------------------------- dedup

#[derive(Args, Debug)]
pub struct DedupArgs {
    /// 扫描根目录
    #[arg(required = true, value_name = "ROOT")]
    pub roots: Vec<PathBuf>,

    /// 最小体积门槛
    #[arg(long = "min-size", default_value = "1")]
    pub min_size: String,

    /// 清理动作：默认只报告
    #[arg(long = "delete", value_name = "dup|keep-first")]
    pub delete: Option<String>,

    /// 用硬链接替代删除
    #[arg(long)]
    pub hardlink: bool,

    /// 排除模式
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
}

// ---------------------------------------------------------------- pack

#[derive(Args, Debug)]
pub struct PackArgs {
    /// 要打包的路径
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 输出文件名（可按扩展名推断格式）
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// 强制格式
    #[arg(short = 'f', long = "format", value_name = "zip|7z|tar|gz|xz")]
    pub format: Option<String>,

    /// 压缩级别 0..9
    #[arg(short = 'l', long, default_value_t = 6)]
    pub level: u8,

    /// zip 加密密码（ZipCrypto）。`-p` 已被「模式」占用，密码只用长选项
    #[arg(long, visible_alias = "pwd")]
    pub password: Option<String>,

    /// 分卷大小（如 100M）
    #[arg(long)]
    pub split: Option<String>,

    /// 归档内根路径映射
    #[arg(long)]
    pub root: Option<PathBuf>,

    #[command(flatten)]
    pub select: SelectArgs,
}

// ---------------------------------------------------------------- unpack

#[derive(Args, Debug)]
pub struct UnpackArgs {
    /// 归档文件，可多个
    #[arg(required = true, value_name = "ARCHIVE")]
    pub archives: Vec<PathBuf>,

    /// 目标目录
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// 拍平
    #[arg(long)]
    pub flat: bool,

    /// 只解匹配的条目
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// 排除条目
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,

    /// zip 解密密码（与 pack 一致：只用长选项）
    #[arg(long, visible_alias = "pwd")]
    pub password: Option<String>,

    /// 列出条目而不解压
    #[arg(short = 'l', long = "list")]
    pub list: bool,

    #[command(flatten)]
    pub conflict: ConflictArgs,
}

// ---------------------------------------------------------------- split / join

#[derive(Args, Debug)]
pub struct SplitArgs {
    /// 要切分的文件
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// 每卷大小（如 100M）
    #[arg(short = 's', long, value_name = "SIZE")]
    pub size: String,

    /// 序号位数
    #[arg(long, default_value_t = 3)]
    pub digits: usize,

    /// 输出前缀（默认原文件名）
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct JoinArgs {
    /// 首个分卷（如 big.bin.001）
    #[arg(value_name = "PART")]
    pub part: PathBuf,

    /// 输出文件
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,
}

// ---------------------------------------------------------------- b64

#[derive(Args, Debug)]
pub struct B64Args {
    /// 输入文件，`-` 表示 stdin
    #[arg(value_name = "INPUT")]
    pub input: PathBuf,

    /// 解码模式
    #[arg(short = 'd', long)]
    pub decode: bool,

    /// URL-safe 字母表
    #[arg(short = 'u', long = "url-safe")]
    pub url_safe: bool,

    /// 省略 `=` 填充
    #[arg(long = "no-pad")]
    pub no_pad: bool,

    /// 输出文件（默认 stdout）
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// 每 N 列换行
    #[arg(long, default_value_t = 0)]
    pub wrap: usize,

    /// 严格校验字符集与长度
    #[arg(long)]
    pub check: bool,
}

// ---------------------------------------------------------------- hash

#[derive(Args, Debug)]
pub struct HashArgs {
    /// 目标文件，可多个；`-` 表示 stdin
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 算法，可逗号分隔多个
    #[arg(short = 'a', long = "algo", default_value = "sha256")]
    pub algo: String,

    /// 校验清单文件
    #[arg(short = 'c', long = "check", value_name = "FILE")]
    pub check: Option<PathBuf>,

    /// 生成校验清单
    #[arg(long = "write", value_name = "FILE")]
    pub write: Option<PathBuf>,

    /// 大写输出
    #[arg(long)]
    pub upper: bool,

    /// Base64 输出摘要
    #[arg(long)]
    pub base64: bool,

    /// 递归处理目录
    #[arg(long)]
    pub recursive: bool,
}

// ---------------------------------------------------------------- replace

#[derive(Args, Debug)]
pub struct ReplaceArgs {
    /// 目标文件或目录
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// 匹配模式（精简正则）或字面量
    #[arg(short = 'p', long, value_name = "PATTERN")]
    pub pattern: String,

    /// 替换文本，支持 `$1..$9` `$0` `$$`
    #[arg(short = 'r', long, default_value = "")]
    pub replacement: String,

    /// 字面量匹配而非正则
    #[arg(long)]
    pub literal: bool,

    /// 大小写不敏感
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// 真正写回磁盘
    #[arg(short = 'w', long)]
    pub write: bool,

    /// 保留原文件备份后缀（如 .bak）
    #[arg(long)]
    pub backup: Option<String>,

    /// 二进制文件也处理
    #[arg(long)]
    pub binary: bool,

    /// 只处理匹配的文件名/相对路径（可多次）
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// 排除模式
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
}

// ---------------------------------------------------------------- cfg

#[derive(Args, Debug)]
pub struct CfgArgs {
    /// 子命令：list / set / get / path / reset
    #[arg(default_value = "list", value_name = "ACTION")]
    pub action: String,

    /// 键名（set/get 用）
    pub key: Option<String>,

    /// 值（set 用）
    pub value: Option<String>,
}

/// 供 help 里展示的格式名列表
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FormatName {
    Zip,
    #[value(name = "7z", alias = "sevenz")]
    SevenZ,
    Tar,
    Gz,
    Xz,
    Rar,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// 回归：`Find` 曾挂 `visible_alias = "find"`（与命令名同名），
    /// clap 的 debug 断言会在解析时直接 panic，而 release 构建不触发——
    /// 这类问题只能在测试里用 `debug_assert()` 兜住。
    #[test]
    fn cli_debug_assert_passes() {
        super::Cli::command().debug_assert();
    }

    /// 常用别名必须仍能用（去掉重复别名时别误删真的别名）。
    #[test]
    fn aliases_still_work() {
        for (cmd, expect) in [("del", "rm"), ("copy", "cp"), ("move", "mv"), ("rename", "rn")] {
            let c = super::Cli::command();
            let sub = c
                .get_subcommands()
                .find(|s| s.get_name() == expect || s.get_all_aliases().any(|a| a == cmd))
                .expect("子命令应存在");
            assert!(
                sub.get_name() == expect && sub.get_all_aliases().any(|a| a == cmd),
                "别名 {cmd} 应指向 {expect}"
            );
        }
    }
}
