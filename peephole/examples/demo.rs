//! A program to try peephole with: `PEEPHOLE_WAIT=1 cargo run --example demo`.

use peephole::Reflect;
use std::collections::HashMap;

#[derive(Reflect, Debug)]
pub struct Item {
    pub name: String,
    pub qty: u32,
    pub price: f64,
}

#[derive(Reflect, Debug)]
pub enum Status {
    Pending,
    Shipped { tracking: String },
}

#[derive(Reflect, Debug)]
pub struct Event {
    pub status: Status,
}

#[derive(Reflect, Debug)]
pub struct Order {
    pub id: u64,
    pub coupon: Option<String>,
    pub items: Vec<Item>,
    pub status: Status,
    secret: String,
}

#[peephole::methods]
impl Item {
    fn total(&self) -> f64 {
        f64::from(self.qty) * self.price
    }
}

#[peephole::methods]
impl Order {
    fn total(&self) -> f64 {
        self.items.iter().map(Item::total).sum()
    }

    /// Not callable from the REPL, `&str` arguments aren't supported.
    fn tagged(&self, tag: &str) -> String {
        format!("{tag}{}", self.id)
    }
}

#[peephole::scope]
fn checkout(order: &mut Order, attempts: &mut u32) {
    let names: HashMap<String, i64> = [("alice".into(), 1), ("bob".into(), 2)].into();
    let history = vec![
        Event {
            status: Status::Pending,
        },
        Event {
            status: Status::Shipped {
                tracking: "OLD1".into(),
            },
        },
    ];
    peephole::inspect!();
}

fn main() {
    let mut order = Order {
        id: 7,
        coupon: None,
        items: vec![
            Item {
                name: "apple".into(),
                qty: 3,
                price: 0.5,
            },
            Item {
                name: "pear".into(),
                qty: 2,
                price: 0.75,
            },
        ],
        status: Status::Shipped {
            tracking: "XY123".into(),
        },
        secret: "hunter2".into(),
    };
    let mut attempts = 1u32;
    if order.items.is_empty() {
        order.status = Status::Pending;
    }
    checkout(&mut order, &mut attempts);
    println!(
        "qty={} attempts={} total={} coupon={:?}",
        order.items[0].qty,
        attempts,
        order.total(),
        order.coupon
    );
    assert_eq!(order.tagged("#"), "#7");
}
