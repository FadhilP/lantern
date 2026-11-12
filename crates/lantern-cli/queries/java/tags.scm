(class_declaration
  name: (identifier) @name) @definition.class

(method_declaration
  name: (identifier) @name) @definition.method

(method_invocation
  name: (identifier) @name
  arguments: (argument_list) @reference.call)

(interface_declaration
  name: (identifier) @name) @definition.interface

(type_list
  (type_identifier) @name) @reference.implementation

(object_creation_expression
  type: (type_identifier) @name) @reference.class

(superclass (type_identifier) @name) @reference.implementation

; Lantern additions
(enum_declaration name: (identifier) @name) @definition.class
(enum_constant name: (identifier) @name) @definition.constant
(record_declaration name: (identifier) @name) @definition.class
(annotation_type_declaration name: (identifier) @name) @definition.interface
(annotation_type_element_declaration name: (identifier) @name) @definition.method
(constructor_declaration name: (identifier) @name) @definition.method
(compact_constructor_declaration name: (identifier) @name) @definition.method
; Only `static final` fields: an injected `orderService` field would rank before `OrderServiceImpl`.
; Each declarator is tagged on its own, so `int A, B;` does not nest `B` in `A`.
(field_declaration
  (modifiers) @modifiers
  declarator: (variable_declarator name: (identifier) @name) @definition.constant
  (#match? @modifiers "\\bstatic\\b")
  (#match? @modifiers "\\bfinal\\b"))
(constant_declaration declarator: (variable_declarator name: (identifier) @name) @definition.constant)
