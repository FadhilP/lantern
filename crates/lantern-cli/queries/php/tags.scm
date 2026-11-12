(namespace_definition
  name: (namespace_name) @name) @definition.module

(interface_declaration
  name: (name) @name) @definition.interface

(trait_declaration
  name: (name) @name) @definition.interface

(class_declaration
  name: (name) @name) @definition.class

(class_interface_clause [(name) (qualified_name)] @name) @reference.implementation

(property_declaration
  (property_element (variable_name (name) @name))) @definition.field

(function_definition
  name: (name) @name) @definition.function

(method_declaration
  name: (name) @name) @definition.function

(object_creation_expression
  [
    (qualified_name (name) @name)
    (variable_name (name) @name)
  ]) @reference.class

(function_call_expression
  function: [
    (qualified_name (name) @name)
    (variable_name (name)) @name
  ]) @reference.call

(scoped_call_expression
  name: (name) @name) @reference.call

(member_call_expression
  name: (name) @name) @reference.call

; constants: const declarations (top level and class), define(), and enum cases

(const_declaration (const_element (name) @name) @definition.constant)

((function_call_expression
  function: (name) @_function
  arguments: (arguments . (argument (string (string_content) @name)))) @definition.constant
  (#eq? @_function "define"))

(enum_case name: (name) @name) @definition.constant

; base classes
(base_clause [(name) @name (qualified_name (name) @name)]) @reference.implementation
