# peephole

Stop a running Rust program and explore its variables from a REPL.

```rust
use peephole::bevy_reflect;

#[derive(peephole::Reflect)]
struct Order { id: u64, items: Vec<Item>, status: Status }

fn checkout(order: &mut Order) {
    let mut retries = 3;
    peephole::inspect!(mut *order, retries);
}
```

```console
$ peephole
waiting for a program to reach peephole::inspect!(…)
stopped at src/main.rs:42 in shop::checkout · pid 4711 · thread main
   40 │ fn checkout(order: &mut Order) {
   41 │     let mut retries = 3;
▶  42 │     peephole::inspect!(mut *order, retries);
   43 │ }
locals:
  order    Order  Order { id: 7, items: [Item { name: "apple", qty: 3, price:…
  retries  i32    3
main.rs:42> order.items[0].qty = 10
main.rs:42> order.total()
6.5
main.rs:42> order.items.map(|i| i.name)
["apple", "pear"]
main.rs:42> .c
```

Input is [Rhai](https://rhai.rs), a small Rust-like scripting language.
Program variables are live: reading shows the current value, assigning
changes the program. Nothing is copied or recompiled.

## Setup

Add the library and derive `Reflect` on the types you want to look at:

```toml
[dependencies]
peephole = "0.1"
```

The derive refers to `bevy_reflect` by name, so each module that derives
needs `use peephole::bevy_reflect;`. If a `build.rs` includes those
modules, add peephole to `[build-dependencies]` too.

`inspect!` stops only while a `peephole` REPL listens. Otherwise it
returns at once, so it can stay in debug builds. Variables are read-only
unless marked `mut`:

```rust
fn handle(device: &Device, dir: &Path, counter: &mut u32, name: &str) {
    let mut retries = 3;
    peephole::inspect!(
        device,                  // read-only, also through `&T`
        mut *counter,            // writable `&mut` parameter
        mut retries,             // writable local, needs a `mut` binding
        dir = dir.to_path_buf(), // any expression, under a name
        name,                    // `&str` is shown as a read-only copy
    );
}
```

Fields of foreign types that don't implement `Reflect` need
`#[reflect(ignore)]`. If their fields are public, bevy_reflect's
[`reflect_remote`](https://docs.rs/bevy_reflect/latest/bevy_reflect/attr.reflect_remote.html)
can mirror them instead.

Functions and methods become callable after `expose`:

```rust
peephole::expose("total", Order::total);
```

`order.total()` then works in the REPL, since the first parameter is an
`Order`.

## The REPL

Run `peephole` and start the program. Tab completes variables, fields and
methods with their types. While typing a path like `order.items[0].qty`, a
grey hint shows its current value.

| Command | |
|---|---|
| `.continue`, `.c` | resume the program and wait for the next stop |
| `.quit`, `.q`, Ctrl-D | resume the program and exit |
| `.where`, `.w` | show the stop again |
| Ctrl-C | clear the line, or interrupt running input |

`Option` fields read as their value, or `None` as `()`. Assigning a value
stores `Some(value)`, assigning `()` stores `None`.

Besides Rhai's core, logic, string, list and math functions, values from
the program support `len()`, `keys()`, `variant()`, `type_name()`,
`to_array()`, `map(|x| …)`, `filter(|x| …)` and `for x in list { … }`.

## Without a terminal

Scripts and tools can drive stopped programs with one-shot commands:

```console
$ peephole eval 'order.id'        # waits up to 30 s for a stop
stopped at src/main.rs:42 in shop::checkout · pid 4711 · thread main
…
7
$ peephole eval -t 5 'order.items[0].qty = 10'
$ peephole continue
continued
$ peephole stop
```

The first command starts a background server that keeps the program
stopped between commands. `stop` ends it and resumes the program. `eval -`
reads the input from stdin, `--json` prints the raw reply. Input still
running at the timeout is interrupted, and the program stays usable.

Exit codes: 0 ok, 1 error in the input, 2 no program stopped, 3 program
disconnected, 124 timed out.

## Environment

| Variable | |
|---|---|
| `PEEPHOLE_WAIT` | `inspect!` waits for a REPL instead of returning |
| `PEEPHOLE_SOCKET` | socket path, default `$XDG_STATE_HOME/peephole/sock` |

## Protocol

Program and client exchange JSON objects, each followed by a NUL byte, over
the Unix socket. The client listens. A program connects when it reaches
`inspect!` and sends:

```json
{"stop": {"file": "src/main.rs", "line": 42, "function": "shop::checkout",
          "pid": 4711, "thread": "main", "cwd": "/home/me/shop",
          "language": "rhai",
          "locals": [{"name": "order", "type": "Order", "preview": "Order { … }"}]}}
```

It then answers requests until it is told to continue:

| Request | Reply |
|---|---|
| `{"eval": CODE}` | `{"value": TEXT, "output": PRINTED, "error": BOOL}` |
| `{"complete": LINE}` | `{"start": BYTE, "completions": [{"text", "display"}]}` |
| `{"preview": PATH}` | `{"value": TEXT}`, or `{}` if it isn't a plain path |
| `{"describe": true}` | `{"stop": …}` with fresh previews |
| `{"interrupt": true}` | none. Stops a running `eval`, which replies `"interrupted"` |
| `{"continue": true}` | `{"continued": true}`, then the program resumes |

Closing the connection also resumes the program.

## How it works

Values are reached through [`bevy_reflect`](https://docs.rs/bevy_reflect).
Scripts hold references into the program's memory, not copies. A write
that may move memory, like replacing a whole `Vec`, invalidates references
taken before it, and using one gives an error instead of reading freed
memory.

## License

MIT
