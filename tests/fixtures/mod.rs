/// Fixture content for main branch
/// Contains simple function definitions
pub const FIXTURE_MAIN: &str = r#"fn main() {
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
"#;

/// Fixture content for modified main branch
/// Contains different function definitions for testing changes
pub const FIXTURE_MAIN_MODIFIED: &str = r#"fn main() {
    println!("Welcome!");
    greet_all(&["Alice", "Bob"]);
    let result = multiply(2, 3);
    println!("2 * 3 = {}", result);
}

fn greet_all(names: &[&str]) {
    for name in names {
        println!("Hello, {}!", name);
    }
}

fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

fn subtract(a: i32, b: i32) -> i32 {
    a - b
}
"#;
