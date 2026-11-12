(struct_specifier name: (type_identifier) @name body:(_)) @definition.class

(declaration type: (union_specifier name: (type_identifier) @name)) @definition.class

(function_declarator declarator: (identifier) @name) @definition.function

(type_definition declarator: (type_identifier) @name) @definition.type

(enum_specifier name: (type_identifier) @name) @definition.type

; Lantern additions
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call

; constants: object-like macros, enumerators, and const globals

(preproc_def name: (identifier) @name value: (_)) @definition.constant

(enumerator name: (identifier) @name) @definition.constant

(translation_unit
  (declaration
    (type_qualifier) @_qualifier
    declarator: (init_declarator declarator: (identifier) @name)) @definition.constant
  (#eq? @_qualifier "const"))
