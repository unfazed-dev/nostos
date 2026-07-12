//! Minimal stdlib stdin prompting — deliberately not `dialoguer`. `nostos
//! init` asks at most two questions (db-url, tables); a raw
//! `print!` + `read_line` covers that without a new dependency.

use std::io::{self, Write};

pub fn prompt(label: &str) -> io::Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub fn prompt_nonempty(label: &str) -> io::Result<String> {
    loop {
        let value = prompt(label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        println!("  (required)");
    }
}
