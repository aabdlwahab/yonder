fn main() -> anyhow::Result<()> {
    remote_server::run_cli(std::env::args_os(), Vec::new())
}
