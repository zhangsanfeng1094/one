//! Optional CLI for manual poking (not required by tests).

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("checkout") => {
            // demo: unit qty tax coupon
            eprintln!("use cargo test; CLI is a stub");
            std::process::exit(2);
        }
        _ => {
            eprintln!("broken-kit: run `cargo test`");
            std::process::exit(2);
        }
    }
}
