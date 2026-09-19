# Veyra Language Specification — 2026 Edition

Veyra source files use UTF-8 and the .vy extension.

Core declarations are let, const, and fn. Control flow includes if/else, while, for/in, return, break, and continue. Values include Null, Bool, Int, Float, String, List, and Map. Type annotations are optional in the compatibility engine and checked at runtime when present.

Modules are loaded with use "path.vy". Semicolons are accepted as statement terminators. Both line and block comments are supported.

Horizon native execution currently covers variables, constants as declarations, arithmetic, comparisons, if/else, while, lists, and selected built-ins. Unsupported constructs remain source-compatible through the compatibility engine.
