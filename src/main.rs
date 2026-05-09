mod config;
mod retrieve;
mod store;

use crate::config::Config;
use std::process::exit;

fn main() {
    let config = match Config::load() {
        Ok(c) => c,
        Err(err) => {
            println!("{}", err);
            exit(1)
        }
    };
    println!("{:?}", config);
}
