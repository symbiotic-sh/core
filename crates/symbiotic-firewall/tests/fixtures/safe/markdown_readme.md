# Project Widget

Widget is a small Rust library for formatting dates.

## Install

```toml
[dependencies]
widget = "0.1"
```

## Usage

```rust
use widget::format;

fn main() {
    let s = format(now());
    println!("{s}");
}
```

See the [documentation](https://github.com/example/widget) for more.

![logo](https://github.com/example/widget-logo.png)
