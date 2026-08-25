fn main() {
    let path = std::env::args().nth(1).unwrap();
    let input = std::fs::read(&path).unwrap();
    amatl::debug_probe(&input);
}
