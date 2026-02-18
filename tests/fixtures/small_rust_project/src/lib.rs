pub struct Calculator {
    pub value: i32,
}

impl Calculator {
    pub fn new() -> Self {
        Self { value: 0 }
    }

    pub fn add(&mut self, n: i32) {
        self.value += n;
    }

    pub fn get(&self) -> i32 {
        self.value
    }
}

pub struct Config {
    pub debug: bool,
}

impl Config {
    pub fn new(debug: bool) -> Self {
        Self { debug }
    }
}
