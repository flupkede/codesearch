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

    /// Walk the entire tree, calling a function for each node
    fn walk_tree<F>(&self, node: Node, callback: &mut F)
    where
        F: FnMut(Node),
    {
        callback(node);

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk_tree(child, callback);
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

        let result = parser.parse(Language::Markdown, source);
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
}
