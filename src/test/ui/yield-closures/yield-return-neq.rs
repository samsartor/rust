#![feature(yield_closures)]

fn main() {
    let _ = || {
        yield "";
        return 0; //~ ERROR: mismatched types
    };
}
