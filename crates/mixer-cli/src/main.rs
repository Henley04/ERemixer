//! OneMix CLI 二进制入口。逻辑在 lib.rs，供 GUI 主应用复用。

fn main() {
    std::process::exit(mixer_cli::run());
}
