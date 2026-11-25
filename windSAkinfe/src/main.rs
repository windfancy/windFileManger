//! windSAkinfe 命令行入口。
//!
//! 真正的实现都在 lib 里（`windsakinfe::cmd::dispatch`），这里只做参数解析
//! 与退出码透传，方便集成测试直接调用库函数。

use clap::Parser;

use windsakinfe::cli::Cli;
use windsakinfe::cmd;

fn main() {
    let cli = Cli::parse();
    let code = cmd::dispatch(cli);
    std::process::exit(code);
}
