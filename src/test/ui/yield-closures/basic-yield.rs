// run-pass

#![feature(yield_closures)]

fn main() {
    let _ = || { yield };
}
