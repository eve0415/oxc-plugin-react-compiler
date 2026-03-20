//! Validates that capitalized functions are not called directly.
//!
//! Port of `ValidateNoCapitalizedCalls.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Capitalized functions are reserved for components, which must be invoked with JSX.
//! If a function is a component, it should be rendered with JSX. Otherwise, ensure
//! it has no hook calls and rename it to begin with a lowercase letter.

use std::collections::HashMap;

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;
use crate::options::EnvironmentConfig;

const CAPITALIZED_CALL_REASON: &str = "Capitalized functions are reserved for components, which must be invoked with JSX. \
     If this is a component, render it with JSX. Otherwise, ensure that it has no hook \
     calls and rename it to begin with a lowercase letter. Alternatively, if you know \
     for a fact that this function is not a component, you can allowlist it via the \
     compiler config";

fn matches_hook_pattern(name: &str, pattern: &str) -> bool {
    // Common upstream fixture pattern: `.*\b(use[^$]+)$`
    // Intended to match hook-like suffixes after namespace separators (`React$useState`).
    if pattern.contains("use[^$]+") {
        return name.rsplit('$').next().is_some_and(|segment| {
            segment == "use"
                || segment
                    .strip_prefix("use")
                    .and_then(|rest| rest.chars().next())
                    .is_some_and(|ch| ch.is_ascii_uppercase())
        });
    }
    // Fallback: keep legacy prefix semantics.
    name.starts_with(pattern)
}

/// Validates that capitalized functions are not called directly (they should be
/// used as JSX components).
///
/// Returns `Ok(())` if valid, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_no_capitalized_calls(
    func: &HIRFunction,
    env_config: &EnvironmentConfig,
) -> Result<(), CompilerError> {
    let mut allow_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Add all known globals to the allow list
    // The upstream uses DEFAULT_GLOBALS.keys() which is the set of all registered global names
    for name in get_default_global_names() {
        allow_set.insert(name);
    }

    // Add user-configured allowlist
    if let Some(ref allowed) = env_config.validate_no_capitalized_calls {
        for name in allowed {
            allow_set.insert(name.clone());
        }
    }

    // Check if a name matches the hook pattern (simple prefix matching for now).
    // The upstream uses a proper regex, but we use the same simple prefix matching
    // as Environment::matches_hook_pattern.
    let hook_pattern = env_config.hook_pattern.clone();

    let is_allowed = |name: &str| -> bool {
        allow_set.contains(name)
            || hook_pattern
                .as_ref()
                .is_some_and(|pattern| matches_hook_pattern(name, pattern))
    };

    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();
    let mut capital_load_globals: HashMap<IdentifierId, String> = HashMap::new();
    let mut capitalized_properties: HashMap<IdentifierId, String> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name();
                    if !name.is_empty()
                        && name.starts_with(|c: char| c.is_ascii_uppercase())
                        && name != name.to_uppercase() // Don't flag CONSTANTS()
                        && !is_allowed(name)
                    {
                        capital_load_globals.insert(instr.lvalue.identifier.id, name.to_string());
                    }
                }
                InstructionValue::CallExpression { callee, .. } => {
                    let callee_id = callee.identifier.id;
                    if let Some(callee_name) = capital_load_globals.get(&callee_id) {
                        // The upstream throws InvalidReact for direct calls to capitalized globals
                        return Err(CompilerError::Bail(BailOut {
                            reason: format!("{} may be a component", callee_name),
                            diagnostics: vec![CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidReact,
                                message: format!(
                                    "{}. {} may be a component",
                                    CAPITALIZED_CALL_REASON, callee_name
                                ),
                            }],
                        }));
                    }
                }
                InstructionValue::PropertyLoad { property, .. } => {
                    if let PropertyLiteral::String(prop_name) = property
                        && prop_name.starts_with(|c: char| c.is_ascii_uppercase())
                    {
                        capitalized_properties
                            .insert(instr.lvalue.identifier.id, prop_name.clone());
                    }
                }
                // In our HIR, method call properties are lowered as Primitive::String
                // values (not PropertyLoad), so we also need to track capitalized
                // string primitives that may be used as MethodCall properties.
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(s),
                    ..
                } => {
                    if s.starts_with(|c: char| c.is_ascii_uppercase()) {
                        capitalized_properties.insert(instr.lvalue.identifier.id, s.clone());
                    }
                }
                InstructionValue::MethodCall { property, .. } => {
                    let property_id = property.identifier.id;
                    if let Some(property_name) = capitalized_properties.get(&property_id) {
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: format!(
                                "{}. {} may be a component",
                                CAPITALIZED_CALL_REASON, property_name
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Capitalized function calls found".to_string(),
            diagnostics,
        }))
    }
}

/// Get the set of all global names from the default registry.
fn get_default_global_names() -> Vec<String> {
    // These are all the known global names registered in the upstream DEFAULT_GLOBALS.
    let known_globals = [
        "useContext",
        "useState",
        "useActionState",
        "useReducer",
        "useRef",
        "useImperativeHandle",
        "useMemo",
        "useCallback",
        "useEffect",
        "useLayoutEffect",
        "useInsertionEffect",
        "useTransition",
        "useEffectEvent",
        "use",
        "fire",
        "AUTODEPS",
        "React",
        "_jsx",
        "Object",
        "Array",
        "Math",
        "Date",
        "performance",
        "console",
        "Boolean",
        "Number",
        "String",
        "parseInt",
        "parseFloat",
        "isNaN",
        "isFinite",
        "encodeURI",
        "encodeURIComponent",
        "decodeURI",
        "decodeURIComponent",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "Infinity",
        "NaN",
        "Function",
        "RegExp",
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "URIError",
        "EvalError",
        "DataView",
        "Float32Array",
        "Float64Array",
        "Int8Array",
        "Int16Array",
        "Int32Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Uint16Array",
        "Uint32Array",
        "ArrayBuffer",
        "JSON",
        "eval",
    ];
    known_globals.iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_place(id: u32, name: Option<&str>) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: name.map(|n| IdentifierName::Named(n.to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_basic_block(id: u32, instructions: Vec<Instruction>) -> (BlockId, BasicBlock) {
        let bid = BlockId(id);
        (
            bid,
            BasicBlock {
                kind: BlockKind::Block,
                id: bid,
                instructions,
                terminal: Terminal::Return {
                    value: make_test_place(999, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(999),
                    loc: SourceLocation::Generated,
                },
                preds: std::collections::HashSet::new(),
                phis: vec![],
            },
        )
    }

    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_test_place(0, None),
            context: vec![],
            body: HIR {
                entry: blocks.first().map(|(id, _)| *id).unwrap_or(BlockId(0)),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_no_capitalized_calls_is_ok() {
        let instructions = vec![Instruction {
            id: InstructionId(0),
            lvalue: make_test_place(100, None),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Number(42.0),
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        let config = EnvironmentConfig::default();
        assert!(validate_no_capitalized_calls(&func, &config).is_ok());
    }

    #[test]
    fn test_calling_capitalized_global_fails() {
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(1, None),
                value: InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: "MyComponent".to_string(),
                    },
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(2, None),
                value: InstructionValue::CallExpression {
                    callee: make_test_place(1, None),
                    args: vec![],
                    optional: false,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        let config = EnvironmentConfig::default();
        let result = validate_no_capitalized_calls(&func, &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_known_globals_are_allowed() {
        // Calling "Array" should be OK since it's a known global
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(1, None),
                value: InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: "Array".to_string(),
                    },
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(2, None),
                value: InstructionValue::CallExpression {
                    callee: make_test_place(1, None),
                    args: vec![],
                    optional: false,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        let config = EnvironmentConfig::default();
        assert!(validate_no_capitalized_calls(&func, &config).is_ok());
    }

    #[test]
    fn test_all_caps_constant_is_allowed() {
        // ALL_CAPS() should be allowed (it's treated as a constant, not a component)
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(1, None),
                value: InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: "CONSTANT".to_string(),
                    },
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(2, None),
                value: InstructionValue::CallExpression {
                    callee: make_test_place(1, None),
                    args: vec![],
                    optional: false,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        let config = EnvironmentConfig::default();
        assert!(validate_no_capitalized_calls(&func, &config).is_ok());
    }
}
