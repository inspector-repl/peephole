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

impl Order {
    fn total(&self) -> f64 {
        self.items.iter().map(|i| f64::from(i.qty) * i.price).sum()
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
    peephole::expose("total", Order::total);
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
}
