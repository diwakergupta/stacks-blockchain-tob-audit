extern crate blockstack_lib;

use blockstack_lib::address::c32::{c32_address, c32_address_decode};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let s_args: Vec<&str> = args.as_slice().iter().map(|s| s.as_str()).collect();
    let prog = s_args[0];

    let usage = || {
        eprintln!("Usage: {} {{encode VERSION STRING | decode PAYLOAD}}", prog);
        std::process::exit(1);
    };

    match s_args[1..] {
        ["encode", version, payload] => {
            let version_num = match version.parse() {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            };
            let res = c32_address(version_num, payload.as_bytes());
            // println!("{:?}", res);
            match res {
                Ok(encoded) => {
                    println!("{}", encoded);
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            }
        }

        ["decode", payload] => {
            let res = c32_address_decode(payload);
            // println!("{:?}", res);
            match res {
                Ok((version, bytes)) => {
                    let decoded: String = match String::from_utf8(bytes) {
                        Ok(decoded) => decoded,
                        Err(err) => {
                            eprintln!("Error: {}", err);
                            std::process::exit(1);
                        }
                    };
                    println!("version: {}", version);
                    println!("decoded: {}", decoded);
                    std::process::exit(0);
                }
                Err(err) => {
                    eprintln!("Error: {}", err);
                    std::process::exit(1);
                }
            }
        }

        _ => {
            usage();
        }
    }
}
