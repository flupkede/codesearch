#![allow(dead_code)]

use super::ChunkKind;
use crate::file::Language;
use tree_sitter::Node;

/// Language-specific code extraction logic
///
/// Each language has different AST node types and conventions for:
/// - Finding definitions (functions, classes, etc.)
/// - Extracting names
/// - Building signatures
/// - Finding docstrings
///
/// This trait allows us to handle multiple languages with proper semantics.
pub trait LanguageExtractor: Send + Sync {
    /// Get the AST node types that represent definitions in this language
    ///
    /// For example:
    /// - Rust: `["function_item", "struct_item", "impl_item", ...]`
    /// - Python: `["function_definition", "class_definition"]`
    fn definition_types(&self) -> &[&'static str];

    /// Extract the name from a definition node
    ///
    /// Returns None if the node has no name (anonymous)
    fn extract_name(&self, node: Node, source: &[u8]) -> Option<String>;

    /// Extract a function/method signature
    ///
    /// Examples:
    /// - Rust: `fn sort<T: Ord>(items: Vec<T>) -> Vec<T>`
    /// - Python: `def process(data: List[str]) -> Dict[str, int]`
    /// - TypeScript: `function compute(x: number): string`
    fn extract_signature(&self, node: Node, source: &[u8]) -> Option<String>;

    /// Extract docstring/documentation comments
    ///
    /// Different languages have different conventions:
    /// - Rust: `/// ` and `/** */`
    /// - Python: First string literal in function/class body
    /// - JavaScript/TypeScript: JSDoc `/** */`
    fn extract_docstring(&self, node: Node, source: &[u8]) -> Option<String>;

    /// Classify a node into a ChunkKind
    fn classify(&self, node: Node) -> ChunkKind;

    /// Check if a node is a definition
    #[allow(dead_code)]
    fn is_definition(&self, node: Node) -> bool {
        self.definition_types().contains(&node.kind())
    }

    /// Build a label for a node (e.g., "Function: foo", "Class: Bar")
    fn build_label(&self, node: Node, source: &[u8]) -> Option<String> {
        let name = self.extract_name(node, source)?;
        let kind = self.classify(node);

        Some(match kind {
            ChunkKind::Function => format!("Function: {}", name),
            ChunkKind::Method => format!("Method: {}", name),
            ChunkKind::Class => format!("Class: {}", name),
            ChunkKind::Struct => format!("Struct: {}", name),
            ChunkKind::Enum => format!("Enum: {}", name),
            ChunkKind::Trait => format!("Trait: {}", name),
            ChunkKind::Interface => format!("Interface: {}", name),
            ChunkKind::Impl => format!("Impl: {}", name),
            ChunkKind::Mod => format!("Module: {}", name),
            ChunkKind::TypeAlias => format!("Type: {}", name),
            ChunkKind::Const => format!("Const: {}", name),
            ChunkKind::Static => format!("Static: {}", name),
            _ => format!("Symbol: {}", name),
        })
    }
}

/// Get the appropriate extractor for a language
pub fn get_extractor(language: Language) -> Option<Box<dyn LanguageExtractor>> {
    match language {
        Language::Rust => Some(Box::new(RustExtractor)),
        Language::Python => Some(Box::new(PythonExtractor)),
        Language::JavaScript | Language::TypeScript => Some(Box::new(TypeScriptExtractor)),
        _ => None,
    }
}

/// Rust language extractor
pub struct RustExtractor;

impl LanguageExtractor for RustExtractor {
    fn definition_types(&self) -> &[&'static str] {
        &[
            "function_item",
            "struct_item",
            "enum_item",
            "impl_item",
            "trait_item",
            "type_item",
            "mod_item",
            "const_item",
            "static_item",
        ]
    }

    fn extract_name(&self, node: Node, source: &[u8]) -> Option<String> {
        // Rust has consistent "name" field for most definitions
        node.child_by_field_name("name")?
            .utf8_text(source)
            .ok()
            .map(String::from)
    }

    fn extract_signature(&self, node: Node, source: &[u8]) -> Option<String> {
        match node.kind() {
            "function_item" => {
                // Build: fn name<T>(params) -> Return
                let mut sig = String::from("fn ");

                // Add name
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                // Add type parameters
                if let Some(type_params) = node.child_by_field_name("type_parameters") {
                    if let Ok(params_text) = type_params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                // Add parameters
                if let Some(params) = node.child_by_field_name("parameters") {
                    if let Ok(params_text) = params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                // Add return type
                if let Some(return_type) = node.child_by_field_name("return_type") {
                    if let Ok(ret_text) = return_type.utf8_text(source) {
                        sig.push_str(" -> ");
                        sig.push_str(ret_text);
                    }
                }

                Some(sig)
            }
            "struct_item" => {
                // Build: struct Name<T>
                let mut sig = String::from("struct ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(type_params) = node.child_by_field_name("type_parameters") {
                    if let Ok(params_text) = type_params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                Some(sig)
            }
            "enum_item" => {
                // Build: enum Name<T>
                let mut sig = String::from("enum ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(type_params) = node.child_by_field_name("type_parameters") {
                    if let Ok(params_text) = type_params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                Some(sig)
            }
            "trait_item" => {
                // Build: trait Name<T>
                let mut sig = String::from("trait ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(type_params) = node.child_by_field_name("type_parameters") {
                    if let Ok(params_text) = type_params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                Some(sig)
            }
            "impl_item" => {
                // Build: impl<T> Trait for Type
                let mut sig = String::from("impl");

                if let Some(type_params) = node.child_by_field_name("type_parameters") {
                    if let Ok(params_text) = type_params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                if let Some(trait_name) = node.child_by_field_name("trait") {
                    if let Ok(trait_text) = trait_name.utf8_text(source) {
                        sig.push(' ');
                        sig.push_str(trait_text);
                        sig.push_str(" for");
                    }
                }

                if let Some(type_name) = node.child_by_field_name("type") {
                    if let Ok(type_text) = type_name.utf8_text(source) {
                        sig.push(' ');
                        sig.push_str(type_text);
                    }
                }

                Some(sig)
            }
            _ => None,
        }
    }

    fn extract_docstring(&self, node: Node, source: &[u8]) -> Option<String> {
        // Look for line_comment or block_comment nodes immediately before this node
        // Tree-sitter includes them as named siblings in some grammars

        // For now, we'll look at the previous siblings
        let parent = node.parent()?;
        let node_index = (0..parent.named_child_count())
            .find(|&i| parent.named_child(i).map(|c| c.id()) == Some(node.id()))?;

        if node_index > 0 {
            if let Some(prev) = parent.named_child(node_index - 1) {
                if prev.kind() == "line_comment" || prev.kind() == "block_comment" {
                    if let Ok(text) = prev.utf8_text(source) {
                        // Check if it's a doc comment (/// or /**)
                        if text.trim_start().starts_with("///")
                            || text.trim_start().starts_with("/**")
                        {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }

        None
    }

    fn classify(&self, node: Node) -> ChunkKind {
        match node.kind() {
            "function_item" => {
                // Check if it's a method (inside impl block)
                if let Some(parent) = node.parent() {
                    if parent.kind() == "declaration_list" {
                        if let Some(grandparent) = parent.parent() {
                            if grandparent.kind() == "impl_item" {
                                return ChunkKind::Method;
                            }
                        }
                    }
                }
                ChunkKind::Function
            }
            "struct_item" => ChunkKind::Struct,
            "enum_item" => ChunkKind::Enum,
            "impl_item" => ChunkKind::Impl,
            "trait_item" => ChunkKind::Trait,
            "type_item" => ChunkKind::TypeAlias,
            "mod_item" => ChunkKind::Mod,
            "const_item" => ChunkKind::Const,
            "static_item" => ChunkKind::Static,
            _ => ChunkKind::Other,
        }
    }
}

/// Python language extractor
pub struct PythonExtractor;

impl LanguageExtractor for PythonExtractor {
    fn definition_types(&self) -> &[&'static str] {
        &["function_definition", "class_definition"]
    }

    fn extract_name(&self, node: Node, source: &[u8]) -> Option<String> {
        node.child_by_field_name("name")?
            .utf8_text(source)
            .ok()
            .map(String::from)
    }

    fn extract_signature(&self, node: Node, source: &[u8]) -> Option<String> {
        match node.kind() {
            "function_definition" => {
                // Build: def name(params) -> Return:
                let mut sig = String::from("def ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(params) = node.child_by_field_name("parameters") {
                    if let Ok(params_text) = params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                if let Some(return_type) = node.child_by_field_name("return_type") {
                    if let Ok(ret_text) = return_type.utf8_text(source) {
                        sig.push_str(" -> ");
                        sig.push_str(ret_text);
                    }
                }

                Some(sig)
            }
            "class_definition" => {
                // Build: class Name(Base):
                let mut sig = String::from("class ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(superclasses) = node.child_by_field_name("superclasses") {
                    if let Ok(bases_text) = superclasses.utf8_text(source) {
                        sig.push_str(bases_text);
                    }
                }

                Some(sig)
            }
            _ => None,
        }
    }

    fn extract_docstring(&self, node: Node, source: &[u8]) -> Option<String> {
        // Python docstrings are the first statement in the body if it's a string
        let body = node.child_by_field_name("body")?;

        let mut cursor = body.walk();
        // Only check first statement
        if let Some(child) = body.named_children(&mut cursor).next() {
            if child.kind() == "expression_statement" {
                // Check if it contains a string
                let mut expr_cursor = child.walk();
                for expr_child in child.named_children(&mut expr_cursor) {
                    if expr_child.kind() == "string" {
                        return expr_child.utf8_text(source).ok().map(String::from);
                    }
                }
            }
        }

        None
    }

    fn classify(&self, node: Node) -> ChunkKind {
        match node.kind() {
            "function_definition" => {
                // Check if it's a method (inside class)
                if let Some(parent) = node.parent() {
                    if parent.kind() == "block" {
                        if let Some(grandparent) = parent.parent() {
                            if grandparent.kind() == "class_definition" {
                                return ChunkKind::Method;
                            }
                        }
                    }
                }
                ChunkKind::Function
            }
            "class_definition" => ChunkKind::Class,
            _ => ChunkKind::Other,
        }
    }
}

/// TypeScript/JavaScript language extractor
pub struct TypeScriptExtractor;

impl LanguageExtractor for TypeScriptExtractor {
    fn definition_types(&self) -> &[&'static str] {
        &[
            "function_declaration",
            "function",
            "method_definition",
            "class_declaration",
            "class",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            // Arrow functions assigned to const
            "lexical_declaration",
            "variable_declaration",
        ]
    }

    fn extract_name(&self, node: Node, source: &[u8]) -> Option<String> {
        // Try name field first
        if let Some(name) = node.child_by_field_name("name") {
            if let Ok(text) = name.utf8_text(source) {
                return Some(text.to_string());
            }
        }

        // For variable declarations, look for identifier
        if node.kind() == "lexical_declaration" || node.kind() == "variable_declaration" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name) = child.child_by_field_name("name") {
                        if let Ok(text) = name.utf8_text(source) {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }

        None
    }

    fn extract_signature(&self, node: Node, source: &[u8]) -> Option<String> {
        match node.kind() {
            "function_declaration" | "function" => {
                let mut sig = String::from("function ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                if let Some(params) = node.child_by_field_name("parameters") {
                    if let Ok(params_text) = params.utf8_text(source) {
                        sig.push_str(params_text);
                    }
                }

                if let Some(return_type) = node.child_by_field_name("return_type") {
                    if let Ok(ret_text) = return_type.utf8_text(source) {
                        sig.push_str(": ");
                        sig.push_str(ret_text);
                    }
                }

                Some(sig)
            }
            "class_declaration" | "class" => {
                let mut sig = String::from("class ");

                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(name_text) = name.utf8_text(source) {
                        sig.push_str(name_text);
                    }
                }

                Some(sig)
            }
            _ => None,
        }
    }

    fn extract_docstring(&self, node: Node, source: &[u8]) -> Option<String> {
        // Look for JSDoc comments (/** */) before the node
        // Similar to Rust approach
        let parent = node.parent()?;
        let node_index = (0..parent.named_child_count())
            .find(|&i| parent.named_child(i).map(|c| c.id()) == Some(node.id()))?;

        if node_index > 0 {
            if let Some(prev) = parent.named_child(node_index - 1) {
                if prev.kind() == "comment" {
                    if let Ok(text) = prev.utf8_text(source) {
                        if text.trim_start().starts_with("/**") {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }

        None
    }

    fn classify(&self, node: Node) -> ChunkKind {
        match node.kind() {
            "function_declaration" | "function" => ChunkKind::Function,
            "method_definition" => ChunkKind::Method,
            "class_declaration" | "class" => ChunkKind::Class,
            "interface_declaration" => ChunkKind::Interface,
            "type_alias_declaration" => ChunkKind::TypeAlias,
            "enum_declaration" => ChunkKind::Enum,
            "lexical_declaration" | "variable_declaration" => {
                // Check if it's an arrow function
                // If so, treat as Function, otherwise Other
                ChunkKind::Function
            }
            _ => ChunkKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_extractor() {
        assert!(get_extractor(Language::Rust).is_some());
        assert!(get_extractor(Language::Python).is_some());
        assert!(get_extractor(Language::JavaScript).is_some());
        assert!(get_extractor(Language::TypeScript).is_some());
        assert!(get_extractor(Language::Markdown).is_none());
    }

    #[test]
    fn test_rust_definition_types() {
        let extractor = RustExtractor;
        let types = extractor.definition_types();

        assert!(types.contains(&"function_item"));
        assert!(types.contains(&"struct_item"));
        assert!(types.contains(&"enum_item"));
        assert!(types.contains(&"impl_item"));
    }

    #[test]
    fn test_python_definition_types() {
        let extractor = PythonExtractor;
        let types = extractor.definition_types();

        assert!(types.contains(&"function_definition"));
        assert!(types.contains(&"class_definition"));
    }
}
