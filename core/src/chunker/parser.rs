#![allow(dead_code)]

use crate::file::Language;
use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser, Tree};

use super::grammar::GrammarManager;

/// Wrapper around tree-sitter parser with language support
pub struct CodeParser {
    parser: Parser,
    grammar_manager: GrammarManager,
}

impl CodeParser {
    /// Create a new code parser
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            grammar_manager: GrammarManager::new(),
        }
    }

    /// Parse source code for a given language
    pub fn parse(&mut self, language: Language, source: &str) -> Result<ParsedCode> {
        // Get grammar for language
        let grammar = self
            .grammar_manager
            .get_grammar(language)
            .ok_or_else(|| anyhow!("No grammar available for {}", language.name()))?;

        // Set language on parser
        self.parser
            .set_language(&grammar)
            .map_err(|e| anyhow!("Failed to set language: {}", e))?;

        // Parse the source code
        let tree = self
            .parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("Failed to parse source code"))?;

        Ok(ParsedCode {
            tree,
            source: source.to_string(),
            language,
        })
    }

    /// Get the grammar manager for direct access
    pub fn grammar_manager(&self) -> &GrammarManager {
        &self.grammar_manager
    }
}

impl Default for CodeParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Represents parsed code with its AST
pub struct ParsedCode {
    tree: Tree,
    source: String,
    language: Language,
}

impl ParsedCode {
    /// Get the root node of the parse tree
    pub fn root_node(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Get the source code
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the language
    pub fn language(&self) -> Language {
        self.language
    }

    /// Get text for a node
    pub fn node_text(&self, node: Node) -> Result<&str> {
        node.utf8_text(self.source.as_bytes())
            .map_err(|e| anyhow!("Failed to get node text: {}", e))
    }

    /// Check if the parse has any errors
    pub fn has_errors(&self) -> bool {
        self.root_node().has_error()
    }

    /// Walk the tree and find all nodes of a given type
    ///
    /// Note: This returns node IDs that can be used to access nodes via the tree
    pub fn find_nodes_by_type(&self, node_type: &str) -> Vec<(usize, usize)> {
        let mut node_positions = Vec::new();
        self.walk_tree(self.root_node(), &mut |node| {
            if node.kind() == node_type {
                // Store byte range instead of node reference
                node_positions.push((node.start_byte(), node.end_byte()));
            }
        });
        node_positions
    }

    /// Walk the entire tree, calling a function for each node.
    ///
    /// Uses an explicit stack to avoid deep recursion on large ASTs.
    fn walk_tree<F>(&self, root: Node, callback: &mut F)
    where
        F: FnMut(Node),
    {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            callback(node);
            // Push children in reverse so leftmost is processed first (LIFO)
            let mut cursor = node.walk();
            let children: Vec<Node> = node.children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
    }
}

/// Helper to check if a node is a definition (function, class, etc.)
#[allow(dead_code)]
pub fn is_definition_node(node: Node) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "function_definition"
            | "function_item"
            | "method_definition"
            | "class_declaration"
            | "class_definition"
            | "struct_item"
            | "enum_item"
            | "impl_item"
            | "trait_item"
            | "type_item"
    )
}

/// Extract the name from a definition node
#[allow(dead_code)]
pub fn extract_node_name<'a>(node: Node, source: &'a [u8]) -> Option<&'a str> {
    // Try to get name from field
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok();
    }

    // Fall back to searching for identifier children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
        ) {
            if let Ok(text) = child.utf8_text(source) {
                return Some(text);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rust_code() {
        let mut parser = CodeParser::new();
        let source = r#"
fn main() {
    println!("Hello, world!");
}
        "#;

        let result = parser.parse(Language::Rust, source);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.language(), Language::Rust);
        assert!(!parsed.has_errors());
    }

    #[test]
    fn test_parse_python_code() {
        let mut parser = CodeParser::new();
        let source = r#"
def hello():
    print("Hello, world!")
        "#;

        let result = parser.parse(Language::Python, source);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.language(), Language::Python);
        assert!(!parsed.has_errors());
    }

    #[test]
    fn test_parse_javascript_code() {
        let mut parser = CodeParser::new();
        let source = r#"
function hello() {
    console.log("Hello, world!");
}
        "#;

        let result = parser.parse(Language::JavaScript, source);
        assert!(result.is_ok());

        let parsed = result.unwrap();
        assert_eq!(parsed.language(), Language::JavaScript);
        assert!(!parsed.has_errors());
    }

    #[test]
    fn test_find_function_nodes_rust() {
        let mut parser = CodeParser::new();
        let source = r#"
fn foo() {}
fn bar() {}
fn baz() {}
        "#;

        let parsed = parser.parse(Language::Rust, source).unwrap();
        let function_positions = parsed.find_nodes_by_type("function_item");

        assert_eq!(function_positions.len(), 3);
    }

    #[test]
    fn test_is_definition_node() {
        let mut parser = CodeParser::new();
        let source = "fn test() {}";

        let parsed = parser.parse(Language::Rust, source).unwrap();
        let root = parsed.root_node();

        let mut found_def = false;
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if is_definition_node(child) {
                found_def = true;
            }
        }

        assert!(found_def);
    }

    #[test]
    fn test_extract_node_name_rust() {
        let mut parser = CodeParser::new();
        let source = "fn hello_world() {}";

        let parsed = parser.parse(Language::Rust, source).unwrap();
        let root = parsed.root_node();

        // Find function node manually
        let mut cursor = root.walk();
        let mut func_node = None;
        for child in root.children(&mut cursor) {
            if child.kind() == "function_item" {
                func_node = Some(child);
                break;
            }
        }

        assert!(func_node.is_some());
        let name = extract_node_name(func_node.unwrap(), source.as_bytes());
        assert_eq!(name, Some("hello_world"));
    }

    #[test]
    fn test_parse_invalid_language() {
        let mut parser = CodeParser::new();
        let source = "some code";

        // Toml has no compiled-in grammar, so parsing must fail.
        let result = parser.parse(Language::Toml, source);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_syntax_error() {
        let mut parser = CodeParser::new();
        let source = "fn incomplete("; // Syntax error

        let result = parser.parse(Language::Rust, source);
        assert!(result.is_ok()); // Parser succeeds even with errors

        let parsed = result.unwrap();
        assert!(parsed.has_errors()); // But marks the tree as having errors
    }

    /// Regression test for #112 at the tree-walker level. `find_nodes_by_type`
    /// drives `walk_tree`, which is now iterative. A ~50k-deep AST (nested
    /// parenthesized expressions) must be walked without overflowing the stack;
    /// we enforce this on a 1 MiB thread stack so any return to recursion fails
    /// the test deterministically.
    #[test]
    fn test_walk_tree_deeply_nested_does_not_overflow() {
        const DEPTH: usize = 50_000;
        let mut source = String::with_capacity(DEPTH * 2 + 32);
        source.push_str("fn deep() -> i32 {\n    ");
        for _ in 0..DEPTH {
            source.push('(');
        }
        source.push('0');
        for _ in 0..DEPTH {
            source.push(')');
        }
        source.push_str("\n}\n");

        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024) // 1 MiB — too small for 50k recursive frames
            .spawn(move || {
                let mut parser = CodeParser::new();
                let parsed = parser.parse(Language::Rust, &source).expect("parse rust");
                // Exercises walk_tree across the entire deep AST.
                parsed.find_nodes_by_type("integer_literal").len()
            })
            .expect("spawn parser thread");

        let count = handle
            .join()
            .expect("walk_tree must not overflow the stack on a deep AST");
        assert!(
            count >= 1,
            "should find the integer literal at the bottom of the nesting"
        );
    }
}
