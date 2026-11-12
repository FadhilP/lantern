(class_declaration name: (identifier) @name) @definition.class

(class_declaration (base_list (_) @name)) @reference.implementation

(interface_declaration name: (identifier) @name) @definition.interface

(interface_declaration (base_list (_) @name)) @reference.implementation

(method_declaration name: (identifier) @name) @definition.method

(object_creation_expression type: (identifier) @name) @reference.class

(type_parameter_constraints_clause (identifier) @name) @reference.class

(type_parameter_constraint (type type: (identifier) @name)) @reference.class

(variable_declaration type: (identifier) @name) @reference.class

(invocation_expression function: (member_access_expression name: (identifier) @name)) @reference.send

(namespace_declaration name: (identifier) @name) @definition.module

(namespace_declaration name: (identifier) @name) @module

; Lantern additions
(struct_declaration name: (identifier) @name) @definition.class
(enum_declaration name: (identifier) @name) @definition.class
(invocation_expression function: (identifier) @name) @reference.call

; constants: const and static readonly fields, and enum members

(field_declaration
  (modifier) @_const
  (variable_declaration (variable_declarator name: (identifier) @name))
  (#eq? @_const "const")) @definition.constant

(field_declaration
  (modifier) @_first
  (modifier) @_second
  (variable_declaration (variable_declarator name: (identifier) @name))
  (#any-of? @_first "static" "readonly")
  (#any-of? @_second "static" "readonly")) @definition.constant

(enum_member_declaration name: (identifier) @name) @definition.constant
