use kurama_cli::{
    app,
    args::{HELP, parse_from},
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
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
    if args.internal_print_prompt_bundle {
        println!("{}", app::prompt_bundle());
        return;
    }

    if let Err(error) = app::run(args).await {
        eprintln!("kurama: {error}");
        std::process::exit(2);
    }
}
