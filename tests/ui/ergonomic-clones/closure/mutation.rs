//@ check-pass

#![feature(ergonomic_clones)]

fn main() {
    let mut my_var = false;
    let mut callback = use || {
        my_var = true;
    };
    callback();
}
