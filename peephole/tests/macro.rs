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

struct Plain(u8);

struct Borrowing<'a>(&'a str);

fn opaque(plain: &Plain, borrowing: &Borrowing<'_>) {
    let mut local = Plain(1);
    peephole::inspect!(plain, borrowing, mut local);
    local.0 += plain.0 + u8::try_from(borrowing.0.len()).unwrap();
    assert_eq!(local.0, 3);
}

#[test]
fn shows_values_without_reflect_as_opaque() {
    let text = String::from("x");
    opaque(&Plain(1), &Borrowing(&text));
}

struct Counter {
    hits: u32,
}

impl Counter {
    #[peephole::scope]
    fn record(&mut self, items: &mut Vec<Config>, owned: Vec<u8>, text: &str) {
        let mut seen = 0u32;
        let plain = Plain(2);
        drop(owned);
        for item in items.iter_mut() {
            seen += 1;
            peephole::inspect!(!owned);
            item.name.push('!');
        }
        match items.first() {
            Some(first) if !first.name.is_empty() => peephole::inspect!(!owned),
            _ => {}
        }
        let check = |n: u32| {
            peephole::inspect!();
            n + 1
        };
        peephole::inspect!(!owned);
        self.hits += check(seen) + u32::from(plain.0) + u32::try_from(text.len()).unwrap();
    }
}

#[test]
fn scope_fills_in_the_variables() {
    let mut counter = Counter { hits: 0 };
    let mut items = vec![Config { name: "a".into() }];
    counter.record(&mut items, vec![1], "xy");
    assert_eq!(counter.hits, 6);
    assert_eq!(items[0].name, "a!");
}
