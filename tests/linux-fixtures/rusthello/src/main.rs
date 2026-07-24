fn main() {
    let mut v: Vec<u32> = Vec::new();
    for i in 1..=4 {
        v.push(i * i);
    }
    let sum: u32 = v.iter().sum();
    let s = format!("rust glibc: squares sum {sum}");
    println!("{s}");
    std::process::exit(7);
}
