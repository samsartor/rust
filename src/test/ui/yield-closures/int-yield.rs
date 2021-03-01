// run-pass

#![feature(yield_closures)]

fn main() {
    let mut x = || {
        yield 0;
        yield 1;
        yield 2;
        3
    };

    assert_eq!(x(), 0);
    assert_eq!(x(), 1);
    assert_eq!(x(), 2);
    assert_eq!(x(), 3);
}
