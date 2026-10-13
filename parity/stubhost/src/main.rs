//! A stand-in executable for the parity suite on Windows: `CreateProcess` cannot run a `.cmd` script, and `cmd /c` cuts an argument at
//! its first newline (a system prompt has many). Copied to `claude.exe` / `gh.exe`, it runs `<name>_stub.py` beside it with the same
//! arguments, using the interpreter named in `<name>_python.txt`.

use std::process::{exit, Command};

fn main() {
    let exe = std::env::current_exe().expect("own path");
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("stub").to_string();
    let script = exe.with_file_name(format!("{stem}_stub.py"));
    let python = std::fs::read_to_string(exe.with_file_name(format!("{stem}_python.txt"))).map(|s| s.trim().to_string()).unwrap_or_else(|_| "python".into());
    match Command::new(&python).arg(&script).args(std::env::args_os().skip(1)).status() {
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("stubhost: cannot run {python} {}: {e}", script.display());
            exit(127);
        }
    }
}
