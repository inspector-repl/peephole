//! `peephole`: a REPL for programs stopped at `peephole::inspect!`.

mod highlight;
mod interrupt;
mod repl;
mod server;
mod view;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None => repl::run(),
        Some("__server") => Ok(server::serve()),
        Some("-h" | "--help" | "help") => {
            print!("{}", server::USAGE);
            Ok(0)
        }
        Some(command) => server::run(command, &args[1..]),
    };
    std::process::exit(result.unwrap_or_else(|err| {
        eprintln!("peephole: {err}");
        1
    }));
}
