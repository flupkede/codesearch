use codesearch::chunker::SemanticChunker;
use codesearch::file::Language;
use std::path::Path;

fn main() {
    println!("=== Semantic Chunking Demo ===\n");

    let mut chunker = SemanticChunker::new(100, 2000, 10);

    // Demo 1: Rust code with various definitions
    println!("📦 Demo 1: Rust Code\n");
    demo_rust(&mut chunker);

    // Demo 2: Python code with classes and methods
    println!("\n📦 Demo 2: Python Code\n");
    demo_python(&mut chunker);

    // Demo 3: TypeScript code with interfaces
    println!("\n📦 Demo 3: TypeScript Code\n");
    demo_typescript(&mut chunker);

    println!("\n✅ Demo complete!\n");
}

fn demo_rust(chunker: &mut SemanticChunker) {
    let rust_code = r#"
//! This is a module-level doc comment

use std::collections::HashMap;

/// A simple Point struct representing 2D coordinates
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    /// Create a new Point at the origin
    fn new() -> Self {
        Point { x: 0.0, y: 0.0 }
    }

    /// Calculate distance from origin
    fn distance(&self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

/// Sort a vector of items in place
///
/// # Examples
///
/// ```
/// let mut items = vec![3, 1, 2];
/// sort(&mut items);
/// assert_eq!(items, vec![1, 2, 3]);
/// ```
fn sort<T: Ord>(items: &mut Vec<T>) {
    items.sort();
}

enum Status {
    Active,
    Inactive,
    Pending,
}

trait Drawable {
    fn draw(&self);
}

const MAX_SIZE: usize = 100;
"#;

    let path = Path::new("example.rs");
    match chunker.chunk_semantic(Language::Rust, path, rust_code) {
        Ok(chunks) => {
            let chunks_count: usize = chunks.len();
            println!("✅ Found {} chunks\n", chunks_count);

            for (i, chunk) in chunks.iter().enumerate() {
                println!("Chunk {} [{:?}]:", i + 1, chunk.kind);
                println!("  Lines: {}-{}", chunk.start_line, chunk.end_line);

                if !chunk.context.is_empty() {
                    println!("  Context: {}", chunk.context.join(" > "));
                }

                if let Some(sig) = &chunk.signature {
                    println!("  Signature: {}", sig);
                }

                if let Some(doc) = &chunk.docstring {
                    let first_line: Option<&str> = doc.lines().next();
                    println!("  Docstring: {}", first_line.unwrap_or(""));
                }

                println!(
                    "  Content preview: {}...",
                    chunk
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                );
                println!();
            }
        }
        Err(e) => println!("❌ Error: {}", e),
    }
}

fn demo_python(chunker: &mut SemanticChunker) {
    let python_code = r#"
import math
from typing import List, Dict

def calculate_average(numbers: List[float]) -> float:
    """Calculate the average of a list of numbers.

    Args:
        numbers: A list of floating point numbers

    Returns:
        The average value
    """
    return sum(numbers) / len(numbers)

class DataProcessor:
    """A class for processing data with various operations."""

    def __init__(self, name: str):
        """Initialize the processor.

        Args:
            name: The name of this processor
        """
        self.name = name
        self.data = []

    def add_data(self, item: Dict[str, int]) -> None:
        """Add an item to the dataset.

        Args:
            item: A dictionary representing a data point
        """
        self.data.append(item)

    def process(self) -> List[Dict[str, int]]:
        """Process all data items.

        Returns:
            A list of processed items
        """
        return [self._transform(item) for item in self.data]

    def _transform(self, item: Dict[str, int]) -> Dict[str, int]:
        """Internal transformation function."""
        return {k: v * 2 for k, v in item.items()}
"#;

    let path = Path::new("example.py");
    match chunker.chunk_semantic(Language::Python, path, python_code) {
        Ok(chunks) => {
            println!("✅ Found {} chunks\n", chunks.len());

            for (i, chunk) in chunks.iter().enumerate() {
                println!("Chunk {} [{:?}]:", i + 1, chunk.kind);
                println!("  Lines: {}-{}", chunk.start_line, chunk.end_line);

                if !chunk.context.is_empty() {
                    println!("  Context: {}", chunk.context.join(" > "));
                }

                if let Some(sig) = &chunk.signature {
                    println!("  Signature: {}", sig);
                }

                if let Some(doc) = &chunk.docstring {
                    let first_line: Option<&str> = doc
                        .lines()
                        .find(|l: &&str| !l.trim().is_empty() && !l.trim().starts_with('*'));
                    println!("  Doc: {}", first_line.unwrap_or("").trim());
                }

                println!();
            }
        }
        Err(e) => println!("❌ Error: {}", e),
    }
}

fn demo_typescript(chunker: &mut SemanticChunker) {
    let typescript_code = r#"
interface User {
    id: number;
    name: string;
    email: string;
}

type UserRole = 'admin' | 'user' | 'guest';

/**
 * Manages user authentication and authorization
 */
class AuthManager {
    private users: Map<number, User>;

    constructor() {
        this.users = new Map();
    }

    /**
     * Register a new user
     * @param user - The user to register
     * @returns The registered user's ID
     */
    registerUser(user: User): number {
        this.users.set(user.id, user);
        return user.id;
    }

    /**
     * Find a user by their ID
     * @param id - The user ID to search for
     * @returns The user if found, undefined otherwise
     */
    findUser(id: number): User | undefined {
        return this.users.get(id);
    }
}

/**
 * Calculate the hash of a string
 * @param input - The string to hash
 * @returns The hash value
 */
function calculateHash(input: string): number {
    let hash = 0;
    for (let i = 0; i < input.length; i++) {
        hash = ((hash << 5) - hash) + input.charCodeAt(i);
        hash = hash & hash;
    }
    return hash;
}

enum Status {
    Pending,
    Active,
    Completed,
    Failed
}
"#;

    let path = Path::new("example.ts");
    match chunker.chunk_semantic(Language::TypeScript, path, typescript_code) {
        Ok(chunks) => {
            let chunks_count: usize = chunks.len();
            println!("✅ Found {} chunks\n", chunks_count);

            for (i, chunk) in chunks.iter().enumerate() {
                println!("Chunk {} [{:?}]:", i + 1, chunk.kind);
                println!("  Lines: {}-{}", chunk.start_line, chunk.end_line);

                if !chunk.context.is_empty() {
                    println!("  Context: {}", chunk.context.join(" > "));
                }

                if let Some(sig) = &chunk.signature {
                    println!("  Signature: {}", sig);
                }

                if let Some(doc) = &chunk.docstring {
                    let first_line: &str = doc
                        .lines()
                        .find(|l: &&str| !l.trim().is_empty() && !l.trim().starts_with('*'))
                        .unwrap_or("");
                    println!("  JSDoc: {}", first_line.trim());
                }

                println!();
            }
        }
        Err(e) => println!("❌ Error: {}", e),
    }
}
