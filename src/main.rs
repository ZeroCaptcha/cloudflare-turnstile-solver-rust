//! Prints a Cloudflare Turnstile token for a page and its sitekey:
//!
//! ```sh
//! export ZEROCAPTCHA_API=https://api.zerocaptcha.io ZEROCAPTCHA_KEY=zc_live_...
//! cargo run -- https://shop.example.com/login 0x4AAAAAAAB1cD2eF3gH4iJ5 [action] [cdata]
//! ```
//!
//! `action` and `cdata` are the widget's `data-action` and `data-cdata` (or `turnstile.render()`'s
//! `action` and `cData` options): pass them whenever the widget sets them, since many sites check
//! both when they verify the token. Set `PROXY_URL` to solve through your own proxy.

use std::process::ExitCode;
use std::time::Duration;

use cloudflare_turnstile_solver::{Client, Task};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=4).contains(&args.len()) {
        eprintln!("Usage: cloudflare-turnstile-solver <page URL> <sitekey> [action] [cdata]");
        return ExitCode::from(2);
    }
    let (Ok(api), Ok(key)) = (
        std::env::var("ZEROCAPTCHA_API"),
        std::env::var("ZEROCAPTCHA_KEY"),
    ) else {
        eprintln!("Set ZEROCAPTCHA_API and ZEROCAPTCHA_KEY first.");
        return ExitCode::from(2);
    };
    let proxy = std::env::var("PROXY_URL")
        .ok()
        .filter(|proxy| !proxy.is_empty());
    let task = Task {
        action: args.get(2).map(String::as_str),
        cdata: args.get(3).map(String::as_str),
        proxy: proxy.as_deref(),
        ..Task::new(&args[0], &args[1])
    };
    match Client::new(api, key).solve(&task, Duration::from_secs(180)) {
        Ok(token) => {
            println!("{token}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
