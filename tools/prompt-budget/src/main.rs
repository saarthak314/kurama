use std::io::{self, Read};

const TOKEN_LIMIT: usize = 2_000;

fn run() -> Result<bool, String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("failed to read prompt bundle: {error}"))?;
    if input.is_empty() {
        return Err("prompt bundle is empty".into());
    }

    let tokenizer = tiktoken_rs::o200k_base()
        .map_err(|error| format!("failed to initialize o200k_base: {error}"))?;
    let count = tokenizer.encode_with_special_tokens(&input).len();
    println!("{count}");
    Ok(count <= TOKEN_LIMIT)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("kurama-prompt-budget: {error}");
            std::process::exit(2);
        }
    }
}
