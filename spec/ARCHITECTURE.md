# Veyra Horizon Architecture

Veyra 2 uses a dual-engine transition architecture.

1. Compatibility runtime preserves the full Veyra 1 language.
2. Native Horizon engine is implemented in Rust: lexer -> parser -> AST -> bytecode compiler -> stack VM.
3. Unified CLI provides project creation, formatting, checking, testing, building, diagnostics, and engine selection.
4. Tooling includes VS Code language support and a reproducible source repository.

The migration target is feature parity in the native engine before compatibility execution is retired.
