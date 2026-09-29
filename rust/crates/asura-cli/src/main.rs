fn main() {
    std::process::exit(asura_cli::app::entry(
        asura_platform::RuntimeDirectory::account,
    ));
}
