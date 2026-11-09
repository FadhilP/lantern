; ADT definitions

(struct_item
    name: (type_identifier) @name) @definition.class

(enum_item
    name: (type_identifier) @name) @definition.class

(union_item
    name: (type_identifier) @name) @definition.class

; type aliases

(type_item
    name: (type_identifier) @name) @definition.class

; function definitions

(function_item
    name: (identifier) @name) @definition.function

; trait definitions
(trait_item
    name: (type_identifier) @name) @definition.interface

; module definitions
(mod_item
    name: (identifier) @name) @definition.module

; macro definitions

(macro_definition
    name: (identifier) @name) @definition.macro

; references

(call_expression
    function: (identifier) @name) @reference.call

(call_expression
    function: (field_expression
        field: (field_identifier) @name)) @reference.call

(call_expression
    function: (scoped_identifier
        name: (identifier) @name)) @reference.call

(call_expression
    function: (generic_function
        function: [
            (identifier) @name
            (scoped_identifier name: (identifier) @name)
            (field_expression field: (field_identifier) @name)
        ])) @reference.call

(macro_invocation
    macro: [
        (identifier) @name
        (scoped_identifier name: (identifier) @name)
    ]) @reference.call

; implementations

(impl_item
    trait: (type_identifier) @name) @reference.implementation

(impl_item
    type: (type_identifier) @name
    !trait) @reference.implementation

; an impl block is the container of its methods, not a symbol

(impl_item
    type: [
        (type_identifier) @name
        (generic_type type: (type_identifier) @name)
        (scoped_type_identifier name: (type_identifier) @name)
    ]) @scope
