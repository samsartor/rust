// check-pass

#![feature(yield_closures)]

fn main() {
    let mut g = || {
        yield "a";
        "b"
    };
    assert_eq!(g(), "a");
    assert_eq!(g(), "b");
    assert_eq!(g(), "a");
}
