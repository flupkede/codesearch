fn main() {
    println!("Hello, world!");
    greet("World");
    let result = add(2, 3);
    println!("2 + 3 = {}", result);
}

fn greet(name: &str) -> String {
    format!("Hello, {}!", name)
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}
