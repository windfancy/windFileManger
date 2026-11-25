//! windSAkinfe —— 批量文件操作瑞士军刀。
//!
//! 所有压缩、哈希、编码、通配匹配算法均为本项目自研实现（需求 C-1），
//! 第三方依赖只有 `clap`（仅用于命令行解析）。

pub mod algo;
pub mod archive;
pub mod cli;
pub mod cmd;
pub mod codec;
pub mod fsx;
pub mod plan;
pub mod term;
pub mod util;
