use kurama_cli::args::{HELP, parse_from};

fn main() {
    let args = match parse_from(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("kurama: {error}");
            std::process::exit(2);
        }
    };

    if args.help {
        print!("{HELP}");
        return;
    }
    if args.version {
        println!("kurama {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    eprintln!("Kurama runtime is not initialized");
    std::process::exit(2);
}
