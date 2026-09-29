use std::path::Path;
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 || args[1] != "--root" || args[3] != "--run-id" {
        eprintln!("bootstrap: InvalidInput");
        std::process::exit(2);
    }
    if let Err(error) = asura_toolchain_bootstrap::preparation::run_operation(
        Path::new(&args[2]),
        &args[4],
        &args[0],
    ) {
        eprintln!("bootstrap: {error}");
        std::process::exit(1);
    }
}
