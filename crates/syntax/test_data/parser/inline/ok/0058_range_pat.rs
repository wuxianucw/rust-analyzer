fn main() {
    match 92 {
        0 ... 100 => (),
        101 ..= 200 => (),
        200 .. 301 => (),
        302 .. => (),
    }
}
