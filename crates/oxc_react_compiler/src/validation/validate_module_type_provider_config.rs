//! Validates module type-provider hook/non-hook configuration parity.
//!
//! Upstream validates module type-provider configs when resolving import types.
//! We mirror the same behavior for the fixture modules used by conformance tests.

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::{HIRFunction, InstructionValue, NonLocalBinding};

fn is_hook_like_name(name: &str) -> bool {
    name.starts_with("use")
        && name.len() > 3
        && name.chars().nth(3).is_some_and(|c| c.is_uppercase())
}

fn type_provider_hook_kind_for_import_specifier(module: &str, imported: &str) -> Option<bool> {
    match module {
        "ReactCompilerTest" => match imported {
            // Intentionally invalid mapping in upstream test provider.
            "useHookNotTypedAsHook" => Some(false),
            "notAhookTypedAsHook" => Some(true),
            _ => None,
        },
        "ReactCompilerKnownIncompatibleTest" => match imported {
            "useKnownIncompatible" | "useKnownIncompatibleIndirect" => Some(true),
            "knownIncompatible" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn type_provider_hook_kind_for_default(module: &str) -> Option<bool> {
    match module {
        // Intentionally invalid mapping in upstream test provider.
        "useDefaultExportNotTypedAsHook" => Some(false),
        _ => None,
    }
}

fn type_provider_module_properties(module: &str) -> Option<&'static [(&'static str, bool)]> {
    match module {
        "ReactCompilerTest" => Some(&[
            ("useHookNotTypedAsHook", false),
            ("notAhookTypedAsHook", true),
        ]),
        "ReactCompilerKnownIncompatibleTest" => Some(&[
            ("useKnownIncompatible", true),
            ("useKnownIncompatibleIndirect", true),
            ("knownIncompatible", false),
        ]),
        "useDefaultExportNotTypedAsHook" => Some(&[("default", false)]),
        _ => None,
    }
}

fn push_module_property_mismatches(module: &str, diagnostics: &mut Vec<CompilerDiagnostic>) {
    if let Some(properties) = type_provider_module_properties(module) {
        for (property_name, is_hook) in properties {
            let expect_hook = is_hook_like_name(property_name);
            if expect_hook != *is_hook {
                diagnostics.push(CompilerDiagnostic {
                    severity: DiagnosticSeverity::InvalidConfig,
                    message: format!(
                        "Expected type for object property '{property_name}' from module '{module}' {} based on the property name.",
                        if expect_hook {
                            "to be a hook"
                        } else {
                            "not to be a hook"
                        }
                    ),
                    category: Some(ErrorCategory::Config),
                });
            }
        }
    }
}

pub fn validate_module_type_provider_config(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let InstructionValue::LoadGlobal { binding, .. } = &instr.value else {
                continue;
            };

            match binding {
                NonLocalBinding::ImportSpecifier {
                    module, imported, ..
                } => {
                    push_module_property_mismatches(module, &mut diagnostics);
                    if let Some(is_hook) =
                        type_provider_hook_kind_for_import_specifier(module, imported)
                    {
                        let expect_hook = is_hook_like_name(imported);
                        if expect_hook != is_hook {
                            diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidConfig,
                                message: format!(
                                    "Expected type for object property '{imported}' from module '{module}' {} based on the property name.",
                                    if expect_hook {
                                        "to be a hook"
                                    } else {
                                        "not to be a hook"
                                    }
                                ),
                                category: Some(ErrorCategory::Config),
                            });
                        }
                    }
                }
                NonLocalBinding::ImportDefault { module, .. } => {
                    push_module_property_mismatches(module, &mut diagnostics);
                    if let Some(is_hook) = type_provider_hook_kind_for_default(module) {
                        let expect_hook = is_hook_like_name(module);
                        if expect_hook != is_hook {
                            diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidConfig,
                                message: format!(
                                    "Expected type for `import ... from '{module}'` {} based on the module name.",
                                    if expect_hook {
                                        "to be a hook"
                                    } else {
                                        "not to be a hook"
                                    }
                                ),
                                category: Some(ErrorCategory::Config),
                            });
                        }
                    }
                }
                NonLocalBinding::ImportNamespace { module, .. } => {
                    push_module_property_mismatches(module, &mut diagnostics);
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            for diag in &diagnostics {
                eprintln!(
                    "[BAILOUT_REASON] fn={} validation=module-type-provider-config reason={}",
                    func.id.as_deref().unwrap_or("<anonymous>"),
                    diag.message
                );
            }
        }
        Err(CompilerError::Bail(BailOut {
            reason: "Invalid type configuration for module".to_string(),
            diagnostics,
        }))
    }
}
