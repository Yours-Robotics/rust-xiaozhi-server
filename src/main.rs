mod server;

use std::{env, io};

fn main() -> io::Result<()> {
    let bind = env::var("RUST_XIAOZHI_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    server::run(&bind)
}
