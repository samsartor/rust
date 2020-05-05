#![feature(generators, yield_closures)]

fn main() {
    let _ = || {
        yield 0;
    };
}
