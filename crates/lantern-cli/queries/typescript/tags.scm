(function_signature
  name: (identifier) @name) @definition.function

(method_signature
  name: (property_identifier) @name) @definition.method

(abstract_method_signature
  name: (property_identifier) @name) @definition.method

(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

(module
  name: (identifier) @name) @definition.module

(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_identifier) @name @reference.type

(new_expression
  constructor: (identifier) @name) @reference.class

; Lantern additions
(type_alias_declaration name: (type_identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.enum
(class_declaration name: (type_identifier) @name) @definition.class
(extends_clause value: (identifier) @name) @reference.implementation
(extends_clause value: (member_expression property: (property_identifier) @name)) @reference.implementation
(implements_clause [(type_identifier) @name (generic_type name: (type_identifier) @name)]) @reference.implementation
(extends_type_clause type: [(type_identifier) @name (generic_type name: (type_identifier) @name)]) @reference.implementation
