# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Rust port of Meta's `babel-plugin-react-compiler` built on the [OXC](https://oxc.rs/) toolchain. The goal is exact behavioral parity with the upstream TypeScript implementation. The current implementation is experimental and will transition from HIR → raw string codegen to HIR → OXC AST-based codegen.

The upstream reference lives at `third_party/react/compiler/packages/babel-plugin-react-compiler/`.

## Build & Test Commands

```bash
# Build everything
cargo build

# Build release (needed for conformance perf)
cargo build --release

# Run core crate unit tests
cargo test --package oxc_react_compiler

# Run a single unit test
cargo test --package oxc_react_compiler -- test_name

# Run conformance suite (the primary correctness metric)
cargo run --release --bin conformance -- --update --include-errors --verbose

# Conformance with filter (run specific fixtures)
cargo run --release --bin conformance -- --include-errors --filter "fixture-name-pattern"

# Conformance with diff output for failures
cargo run --release --bin conformance -- --include-errors --diff

# Near-miss analysis (find almost-passing fixtures)
cargo run --release --bin conformance -- --include-errors --near-miss

# Show full actual vs expected for failures
cargo run --release --bin conformance -- --include-errors --show
```

## Conformance Test Details

- Fixtures live in `third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/`
- Each fixture is a `.js`/`.ts`/`.tsx` file paired with a `.expect.md` containing expected output
- `--include-errors` includes error-expecting fixtures (otherwise they're skipped)
- `--update` rewrites the snapshot file at `tasks/conformance/snapshots/react_compiler.snap.md`
- Results have ±1-2 non-determinism due to Rayon parallel execution and hashmap ordering
- The conformance runner applies 56+ normalization passes to handle formatting differences between Rust string codegen and Babel codegen

## Architecture

### Compilation Pipeline (7 phases, ~72 passes)

Defined in `pipeline.rs`, this mirrors upstream's `Pipeline.ts`:

1. **HIR Construction** — OXC AST → CFG-based HIR (`hir/build.rs`)
2. **HIR Pre-processing** — prune_maybe_throws, drop_manual_memoization, inline_iifes, merge_consecutive_blocks
3. **SSA + Analysis** — enter_ssa, eliminate_redundant_phi, constant_propagation, infer_types
4. **Mutation/Aliasing** — analyse_functions, infer_mutation_aliasing_effects, dead_code_elimination
5. **Validation** — hooks usage, ref access, setState-in-render, etc.
6. **Reactive Scope Construction** — infer reactive places, build scope terminals, flatten loops/hooks, propagate dependencies
7. **Reactive Function + Codegen** — build_reactive_function (CFG → tree IR), scope alignment/merging/pruning, codegen_reactive (emit JS)

### Data Flow

```
Source → oxc_parser → OXC AST
  → oxc_semantic + hir::build → HIR (CFG with BasicBlocks, Terminals, Instructions)
  → SSA passes → HIR in SSA form
  → Aliasing analysis → HIR with mutation ranges
  → Reactive scope inference → HIR with ReactiveScope annotations
  → build_reactive_function → ReactiveFunction (tree-shaped IR)
  → codegen_reactive → JavaScript output with memoization
```

### Core IR Types (in `hir/types.rs`)

- **HIRFunction** — function lowered to CFG form (blocks, terminals, phis)
- **BasicBlock** — instructions + terminal (Goto, If, For, Return, etc.)
- **Instruction** — single operation with lvalue (Place) and value (InstructionValue enum)
- **Place** — identifier + effect (Read, Mutate, Freeze, etc.) + reactive flag
- **ReactiveFunction** — tree-shaped IR for codegen (ReactiveBlock → Vec<ReactiveStatement>)
- **ReactiveScope** — memoized scope with dependencies, declarations, and cache slot info

### Key Modules

| Module | Responsibility |
|--------|---------------|
| `hir/build.rs` | Lowers OXC AST → HIR CFG (largest file) |
| `hir/types.rs` | All IR data structures |
| `hir/globals.rs` | Known global function/type database |
| `hir/propagate_scope_dependencies_hir.rs` | Scope dependency computation |
| `hir/collect_hoistable_property_loads.rs` | Hoistable property analysis |
| `pipeline.rs` | Pass orchestration (second largest file) |
| `reactive_scopes/codegen_reactive.rs` | String-based JS codegen (largest file, future: migrate to OXC AstBuilder) |
| `reactive_scopes/build_reactive_function.rs` | HIR CFG → tree-shaped ReactiveFunction |
| `reactive_scopes/codegen.rs` | Alternative OXC AST-based codegen (in progress) |
| `optimization/constant_propagation.rs` | SSA constant folding |
| `optimization/dead_code_elimination.rs` | DCE pass |
| `inference/infer_mutation_aliasing_effects.rs` | Aliasing side-effect analysis |
| `options.rs` | PluginOptions, EnvironmentConfig, CompilationMode |

### Crate Structure

- **`crates/oxc_react_compiler/`** — Core compiler (the main crate)
- **`crates/oxc_react_compiler_napi/`** — N-API bindings exposing `transform(filename, source, options?)` to Node.js
- **`napi/`** — Published JS wrapper + Vite v8 plugin (`vite.js`)
- **`tasks/conformance/`** — Conformance test runner binary

## Public API

Single entry point: `oxc_react_compiler::compile(filename, source, options) -> CompileResult`

Returns `{ transformed: bool, code: String, map: Option<String> }`.

## Upstream Reference

Each Rust module corresponds to an upstream TypeScript file. When debugging a pass, compare against the upstream source in `third_party/react/compiler/packages/babel-plugin-react-compiler/src/`. Key mappings:

- `pipeline.rs` ↔ `Entrypoint/Pipeline.ts`
- `hir/build.rs` ↔ `HIR/BuildHIR.ts`
- `hir/types.rs` ↔ `HIR/HIR.ts`
- `reactive_scopes/codegen_reactive.rs` ↔ `ReactiveScopes/CodegenReactiveFunction.ts`
- `reactive_scopes/build_reactive_function.rs` ↔ `ReactiveScopes/BuildReactiveFunction.ts`
- `optimization/constant_propagation.rs` ↔ `Optimization/ConstantPropagation.ts`

## Development Notes

- Rust edition 2024, OXC v0.116.0
- `codegen_reactive.rs` uses raw string building; this will be migrated to OXC's AstBuilder for proper AST-based codegen
- Fixture pragmas (first line comments like `// @flow`, `// @compilationMode "all"`) control per-fixture compiler options
- The conformance runner's normalization layer compensates for cosmetic differences (whitespace, semicolons, trailing commas) between Rust string codegen and Babel output
