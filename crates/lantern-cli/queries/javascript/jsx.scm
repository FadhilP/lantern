; JavaScript and TSX only: the TypeScript grammar has no JSX nodes.
; A lowercase name is an HTML tag, not a component. Closing tags are not tagged, so one element is one reference.
(jsx_opening_element name: (identifier) @name (#match? @name "^[A-Z]")) @reference.call
(jsx_self_closing_element name: (identifier) @name (#match? @name "^[A-Z]")) @reference.call
(jsx_opening_element name: (member_expression property: (property_identifier) @name)) @reference.call
(jsx_self_closing_element name: (member_expression property: (property_identifier) @name)) @reference.call
