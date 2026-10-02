//! One table entry per language. Adding a language is a `LangSpec`, never a
//! new engine — the same rule the Python side follows, so the two stay in
//! lockstep and the differential test can prove it.

use tree_sitter::Language;

#[derive(Clone, Copy, PartialEq)]
pub enum ImportStyle {
    Python,
    TsLike,
    Go,
    Rust,
    Java,
    CSharp,
    Ruby,
    CLike,
    Php,
}

#[derive(Clone, Copy, PartialEq)]
pub enum NameStyle {
    /// the `name` field, unwrapping decorated definitions
    Python,
    /// `name`, or the first variable_declarator inside a lexical declaration
    TsLike,
    Go,
    Rust,
    /// follow declarator chains (C, C++)
    CLike,
    /// plain `name` field
    Field,
}

#[derive(Clone, Copy, PartialEq)]
pub enum SigStyle {
    /// declaration text to the body, trailing ':' stripped, uncapped
    Python,
    /// whitespace-collapsed, capped at 200, trailing '{=' stripped
    TsLike,
    /// whitespace-collapsed, capped at 200, trailing '{=:;' stripped
    Generic,
}

pub struct LangSpec {
    pub name: &'static str,
    pub exts: &'static [&'static str],
    pub decls: &'static [(&'static str, &'static str)],
    pub containers: &'static [&'static str],
    pub import_types: &'static [&'static str],
    pub heritage_field: Option<&'static str>,
    pub imports: ImportStyle,
    pub names: NameStyle,
    pub sig: SigStyle,
}

impl LangSpec {
    pub fn kind_of(&self, node_kind: &str) -> Option<&'static str> {
        self.decls.iter().find(|(k, _)| *k == node_kind).map(|(_, v)| *v)
    }
    pub fn is_container(&self, node_kind: &str) -> bool {
        self.containers.contains(&node_kind)
    }
    pub fn is_import(&self, node_kind: &str) -> bool {
        self.import_types.contains(&node_kind)
    }
}

const TS_DECLS: &[(&str, &str)] = &[
    ("function_declaration", "function"),
    ("generator_function_declaration", "function"),
    ("class_declaration", "class"),
    ("abstract_class_declaration", "class"),
    ("interface_declaration", "interface"),
    ("type_alias_declaration", "type"),
    ("enum_declaration", "enum"),
    ("lexical_declaration", "variable"),
    ("variable_declaration", "variable"),
    ("export_statement", "definition"),
];

const JS_DECLS: &[(&str, &str)] = &[
    ("function_declaration", "function"),
    ("generator_function_declaration", "function"),
    ("class_declaration", "class"),
    ("lexical_declaration", "variable"),
    ("variable_declaration", "variable"),
    ("export_statement", "definition"),
];

const C_DECLS: &[(&str, &str)] = &[
    ("function_definition", "function"),
    ("struct_specifier", "struct"),
    ("union_specifier", "union"),
    ("enum_specifier", "enum"),
    ("type_definition", "type"),
    ("declaration", "declaration"),
];

const CPP_DECLS: &[(&str, &str)] = &[
    ("function_definition", "function"),
    ("struct_specifier", "struct"),
    ("union_specifier", "union"),
    ("enum_specifier", "enum"),
    ("type_definition", "type"),
    ("declaration", "declaration"),
    ("class_specifier", "class"),
    ("alias_declaration", "type"),
    ("template_declaration", "definition"),
    ("concept_definition", "concept"),
];

pub fn specs() -> &'static [LangSpec] {
    &[
        LangSpec {
            name: "python",
            exts: &[".py", ".pyi"],
            decls: &[
                ("function_definition", "function"),
                ("class_definition", "class"),
                ("decorated_definition", "definition"),
            ],
            containers: &[],
            import_types: &["import_statement", "import_from_statement", "future_import_statement"],
            heritage_field: Some("superclasses"),
            imports: ImportStyle::Python,
            names: NameStyle::Python,
            sig: SigStyle::Python,
        },
        LangSpec {
            name: "typescript",
            exts: &[".ts", ".mts", ".cts"],
            decls: TS_DECLS,
            containers: &[],
            import_types: &["import_statement"],
            heritage_field: None,
            imports: ImportStyle::TsLike,
            names: NameStyle::TsLike,
            sig: SigStyle::TsLike,
        },
        LangSpec {
            name: "tsx",
            exts: &[".tsx"],
            decls: TS_DECLS,
            containers: &[],
            import_types: &["import_statement"],
            heritage_field: None,
            imports: ImportStyle::TsLike,
            names: NameStyle::TsLike,
            sig: SigStyle::TsLike,
        },
        LangSpec {
            name: "javascript",
            exts: &[".js", ".jsx", ".mjs", ".cjs"],
            decls: JS_DECLS,
            containers: &[],
            import_types: &["import_statement"],
            heritage_field: None,
            imports: ImportStyle::TsLike,
            names: NameStyle::TsLike,
            sig: SigStyle::TsLike,
        },
        LangSpec {
            name: "go",
            exts: &[".go"],
            decls: &[
                ("function_declaration", "function"),
                ("method_declaration", "method"),
                ("type_spec", "type"),
                ("var_spec", "variable"),
                ("const_spec", "constant"),
            ],
            containers: &["type_declaration", "var_declaration", "const_declaration"],
            import_types: &["import_declaration", "package_clause"],
            heritage_field: None,
            imports: ImportStyle::Go,
            names: NameStyle::Go,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "rust",
            exts: &[".rs"],
            decls: &[
                ("function_item", "function"),
                ("function_signature_item", "function"),
                ("struct_item", "struct"),
                ("enum_item", "enum"),
                ("union_item", "union"),
                ("trait_item", "trait"),
                ("impl_item", "impl"),
                ("mod_item", "module"),
                ("const_item", "constant"),
                ("static_item", "constant"),
                ("type_item", "type"),
                ("macro_definition", "macro"),
            ],
            containers: &[],
            import_types: &["use_declaration", "extern_crate_declaration"],
            heritage_field: None,
            imports: ImportStyle::Rust,
            names: NameStyle::Rust,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "java",
            exts: &[".java"],
            decls: &[
                ("class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("enum_declaration", "enum"),
                ("record_declaration", "record"),
                ("annotation_type_declaration", "annotation"),
            ],
            containers: &[],
            import_types: &["import_declaration", "package_declaration"],
            heritage_field: None,
            imports: ImportStyle::Java,
            names: NameStyle::Field,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "c_sharp",
            exts: &[".cs"],
            decls: &[
                ("class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("struct_declaration", "struct"),
                ("enum_declaration", "enum"),
                ("record_declaration", "record"),
                ("record_struct_declaration", "record"),
                ("delegate_declaration", "delegate"),
            ],
            containers: &["namespace_declaration"],
            import_types: &["using_directive", "file_scoped_namespace_declaration", "global_statement"],
            heritage_field: None,
            imports: ImportStyle::CSharp,
            names: NameStyle::Field,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "ruby",
            exts: &[".rb"],
            decls: &[
                ("class", "class"),
                ("module", "module"),
                ("method", "function"),
                ("singleton_method", "function"),
            ],
            containers: &[],
            import_types: &[],
            heritage_field: None,
            imports: ImportStyle::Ruby,
            names: NameStyle::Field,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "c",
            exts: &[".c", ".h"],
            decls: C_DECLS,
            containers: &["preproc_ifdef", "preproc_if", "linkage_specification"],
            import_types: &["preproc_include", "preproc_def", "preproc_function_def", "preproc_call"],
            heritage_field: None,
            imports: ImportStyle::CLike,
            names: NameStyle::CLike,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "cpp",
            exts: &[".cc", ".cpp", ".cxx", ".hpp", ".hh", ".hxx"],
            decls: CPP_DECLS,
            containers: &["namespace_definition", "preproc_ifdef", "preproc_if", "linkage_specification"],
            import_types: &[
                "preproc_include", "preproc_def", "preproc_function_def", "preproc_call",
                "using_declaration",
            ],
            heritage_field: None,
            imports: ImportStyle::CLike,
            names: NameStyle::CLike,
            sig: SigStyle::Generic,
        },
        LangSpec {
            name: "php",
            exts: &[".php"],
            decls: &[
                ("function_definition", "function"),
                ("class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("trait_declaration", "trait"),
                ("enum_declaration", "enum"),
            ],
            containers: &["namespace_definition"],
            import_types: &["namespace_use_declaration", "php_tag", "text_interpolation"],
            heritage_field: None,
            imports: ImportStyle::Php,
            names: NameStyle::Field,
            sig: SigStyle::Generic,
        },
    ]
}

pub fn spec_for_ext(ext: &str) -> Option<&'static LangSpec> {
    specs().iter().find(|spec| spec.exts.contains(&ext))
}

pub fn language_of(name: &str) -> Language {
    match name {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c_sharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        other => panic!("no grammar registered for {other}"),
    }
}
