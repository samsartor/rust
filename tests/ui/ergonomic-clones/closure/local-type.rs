// Check that using the parameter name in its type does not ICE.

#![feature(ergonomic_clones)]

fn main() {
    let _ = use |x: x| x; //~ ERROR expected type
    let _ = use |x: bool| -> x { x }; //~ ERROR expected type
}
