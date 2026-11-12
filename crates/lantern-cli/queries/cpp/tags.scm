(struct_specifier name: (type_identifier) @name body:(_)) @definition.class

(declaration type: (union_specifier name: (type_identifier) @name)) @definition.class

(function_declarator declarator: (identifier) @name) @definition.function

(function_declarator declarator: (field_identifier) @name) @definition.function

(function_declarator declarator: (qualified_identifier scope: (namespace_identifier) @container name: (identifier) @name)) @definition.method

(type_definition declarator: (type_identifier) @name) @definition.type

(enum_specifier name: (type_identifier) @name) @definition.type

(class_specifier name: (type_identifier) @name) @definition.class

; Lantern additions
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
(call_expression function: (qualified_identifier name: (identifier) @name)) @reference.call
(call_expression function: (template_function name: (identifier) @name)) @reference.call
(new_expression type: (type_identifier) @name) @reference.class

; constants: object-like macros, enumerators, and const or constexpr namespace and static
; class members

(preproc_def name: (identifier) @name value: (_)) @definition.constant

(enumerator name: (identifier) @name) @definition.constant

(translation_unit
  (declaration
    (type_qualifier) @_qualifier
    declarator: (init_declarator declarator: (identifier) @name)) @definition.constant
  (#any-of? @_qualifier "const" "constexpr"))

(declaration_list
  (declaration
    (type_qualifier) @_qualifier
    declarator: (init_declarator declarator: (identifier) @name)) @definition.constant
  (#any-of? @_qualifier "const" "constexpr"))

(field_declaration_list
  (field_declaration
    (storage_class_specifier) @_storage
    (type_qualifier) @_qualifier
    declarator: (field_identifier) @name) @definition.constant
  (#eq? @_storage "static")
  (#any-of? @_qualifier "const" "constexpr"))

; base classes
(base_class_clause [(type_identifier) @name (qualified_identifier name: (type_identifier) @name)]) @reference.implementation
