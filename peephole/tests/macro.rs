//! `inspect!` accepts the argument forms real code needs. Without a
//! listening REPL it returns at once, so these only check that they build.

use peephole::Reflect;

#[derive(Reflect)]
struct Config {
    name: String,
}

fn by_reference(config: &Config, counter: &mut u32, text: &str) {
    let owned = vec![1u8, 2];
    let mut changing = 0i64;
    let line: &str = text.trim();
    peephole::inspect!(config, mut *counter, owned, mut changing, label = config.name.len(), text, line,);
    changing += 1;
    assert_eq!(changing, 1);
}

#[test]
fn accepts_shared_mutable_and_named_arguments() {
    let mut counter = 0;
    let borrowed = String::from(" y ");
    by_reference(&Config { name: "x".into() }, &mut counter, &borrowed);
    peephole::inspect!();
}
