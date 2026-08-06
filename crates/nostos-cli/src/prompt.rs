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

/// Prompt with a default; empty input (including EOF, since `read_line`
/// leaves the buffer empty at EOF too) returns `default`.
pub fn prompt_default(label: &str, default: &str) -> io::Result<String> {
    let value = prompt(label)?;
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}
