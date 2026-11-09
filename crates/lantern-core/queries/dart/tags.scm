; Adapted from the tags.scm in tree-sitter-dart. Members are tagged on the node that holds
; the body, so a call in a body gets the method as its enclosing definition. Each variable
; is tagged on its own node, so `var a, b` does not nest `b` in `a`.

(class_declaration name: (identifier) @name) @definition.class
(mixin_application_class (identifier) @name) @definition.class
(mixin_declaration name: (identifier) @name) @definition.class
(extension_declaration name: (identifier) @name) @definition.class
(extension_type_declaration name: (extension_type_name (identifier) @name)) @definition.class
(enum_declaration name: (identifier) @name) @definition.enum
(enum_entry name: (identifier) @name) @definition.constant
(type_alias (type_identifier) @name) @definition.type

(function_declaration signature: (function_signature name: (identifier) @name)) @definition.function
(getter_declaration signature: (getter_signature name: (identifier) @name)) @definition.function
(setter_declaration signature: (setter_signature name: (identifier) @name)) @definition.function
(external_function_declaration signature: (function_signature name: (identifier) @name)) @definition.function
(external_getter_declaration signature: (getter_signature name: (identifier) @name)) @definition.function
(external_setter_declaration signature: (setter_signature name: (identifier) @name)) @definition.function
(local_function_declaration (function_signature name: (identifier) @name)) @definition.function

(top_level_variable_declaration
  [
    (static_final_declaration_list (static_final_declaration name: (identifier) @name) @definition.variable)
    (initialized_identifier_list (initialized_identifier name: (identifier) @name) @definition.variable)
  ])
(external_variable_declaration (identifier_list (identifier) @name @definition.variable))

(declaration
  [
    (static_final_declaration_list (static_final_declaration name: (identifier) @name) @definition.field)
    (initialized_identifier_list (initialized_identifier name: (identifier) @name) @definition.field)
    (identifier_list (identifier) @name @definition.field)
  ])

; `Shop.named()` has two name fields. The anchor keeps the last one, which is next to the parameters.
(method_declaration
  signature: (method_signature
    [
      (function_signature name: (identifier) @name)
      (getter_signature name: (identifier) @name)
      (setter_signature name: (identifier) @name)
      (operator_signature operator: _ @name)
      (constructor_signature name: (identifier) @name . parameters: (_))
      (constant_constructor_signature name: (identifier) @name . parameters: (_))
      (factory_constructor_signature name: (identifier) @name . parameters: (_))
    ])) @definition.method

(declaration
  [
    (function_signature name: (identifier) @name)
    (getter_signature name: (identifier) @name)
    (setter_signature name: (identifier) @name)
    (operator_signature operator: _ @name)
    (constructor_signature name: (identifier) @name . parameters: (_))
    (constant_constructor_signature name: (identifier) @name . parameters: (_))
    (factory_constructor_signature name: (identifier) @name . parameters: (_))
    (redirecting_factory_constructor_signature name: (identifier) @name . parameters: (_))
  ]) @definition.method

(primary_constructor name: (identifier) @name) @definition.method

(call_expression
  function: [
    (identifier) @name
    (member_expression property: (identifier) @name)
    (null_aware_member_expression property: (identifier) @name)
    (static_member_shorthand (identifier) @name)
    (instantiation_expression
      function: [
        (identifier) @name
        (member_expression property: (identifier) @name)
        (null_aware_member_expression property: (identifier) @name)
      ])
  ]) @reference.call

(cascade_call_expression property: (identifier) @name) @reference.call

(type_identifier) @name @reference.type

; The anchors skip an import prefix: `p.Base<int>` implements `Base`, not `p`.
(superclass type: (type (type_identifier) @name . (type_arguments)? .)) @reference.implementation
(mixin_application (type (type_identifier) @name . (type_arguments)? .)) @reference.implementation
(mixins (type (type_identifier) @name . (type_arguments)? .)) @reference.implementation
(interfaces (type (type_identifier) @name . (type_arguments)? .)) @reference.implementation

(combinator "show" (identifier) @name) @reference.import
(import_specification alias: (identifier) @name) @reference.import
