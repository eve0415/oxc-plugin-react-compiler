//! # oxc_react_compiler
//!
//! Rust port of [babel-plugin-react-compiler](https://github.com/facebook/react/tree/main/compiler/packages/babel-plugin-react-compiler)
//! by Meta Platforms, Inc.
//!
//! This crate implements the React Compiler pipeline using the OXC ecosystem:
//! - `oxc_parser` for JavaScript/TypeScript parsing
//! - `oxc_semantic` for scope and symbol analysis
//! - `oxc_codegen` for output generation

mod codegen_backend;
pub(crate) mod environment;
pub(crate) mod error;
pub(crate) mod hir;
pub(crate) mod inference;
pub(crate) mod optimization;
pub mod options;
pub(crate) mod pipeline;
pub(crate) mod reactive_scopes;
pub(crate) mod source_lines;
pub(crate) mod ssa;
pub(crate) mod type_inference;
pub(crate) mod validation;

/// Compile a single file. Returns the transformed code and source map if compilation
/// was applied, or `None` if the file was not transformed (e.g., no components/hooks found).
pub fn compile(filename: &str, source: &str, options: &options::PluginOptions) -> CompileResult {
    crate::optimization::dead_code_elimination::clear_preserved_top_level_let_initializers();
    pipeline::compile(filename, source, options)
}

/// Result of compiling a file.
pub struct CompileResult {
    /// Whether the source was transformed.
    pub transformed: bool,
    /// The output code (original if not transformed).
    pub code: String,
    /// Source map JSON string, if generated.
    pub map: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_fixture() {
        let source = r#"function foo() {}
function Component(props) {
  const a = [];
  const b = {};
  foo(a, b);
  let _ = <div a={a} />;
  foo(b);
  return <div a={a} b={b} />;
}"#;

        let options = options::PluginOptions::default();
        let result = compile("test.js", source, &options);

        eprintln!("transformed: {}", result.transformed);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");

        assert!(result.transformed, "should transform the component");
    }

    #[test]
    fn test_simple_component() {
        let source = r#"function Component(props) {
  const ref = useRef(null);
  return <Foo ref={ref} />;
}"#;
        let options = options::PluginOptions::default();
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }

    #[test]
    fn test_conditional_set_state_in_render_bails_out() {
        let source = r#"function Component(props) {
  const [x, setX] = useState(0);

  const foo = () => {
    setX(1);
  };

  if (props.cond) {
    setX(2);
    foo();
  }

  return x;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: ['TodoAdd'],
  isComponent: 'TodoAdd',
};"#;
        let options = options::PluginOptions::default();
        let result = compile("conditional-set-state-in-render.js", source, &options);
        assert!(
            !result.transformed,
            "conditional set-state fixture should bail out"
        );
    }

    #[test]
    fn test_do_while_loop() {
        let source = r#"function Component() {
  let x = [1, 2, 3];
  let ret = [];
  do {
    let item = x.pop();
    ret.push(item * 2);
  } while (x.length);
  return ret;
}"#;
        // Debug: Build HIR and dump blocks
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs().with_jsx(true);
        let parser_ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        let semantic_ret = oxc_semantic::SemanticBuilder::new().build(&parser_ret.program);
        let semantic = semantic_ret.semantic;
        // Find the Component function
        if let oxc_ast::ast::Statement::FunctionDeclaration(func) = &parser_ret.program.body[0] {
            let body = func.body.as_ref().unwrap();
            let cx = hir::build::LoweringContext::new(
                &semantic,
                source,
                crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            );
            let lower_result = hir::build::lower_function(
                body,
                &func.params,
                cx,
                hir::build::LowerFunctionOptions::function(
                    Some("Component"),
                    func.span,
                    func.generator,
                    func.r#async,
                ),
            )
            .unwrap();
            let hir_func = &lower_result.func;
            eprintln!("---HIR BLOCKS---");
            for (bid, block) in &hir_func.body.blocks {
                eprintln!(
                    "Block {:?}: {} instructions, terminal={:?}",
                    bid,
                    block.instructions.len(),
                    std::mem::discriminant(&block.terminal)
                );
                for (i, instr) in block.instructions.iter().enumerate() {
                    eprintln!(
                        "  [{}] lv_id={:?} lv_name={:?} value={:?}",
                        i,
                        instr.lvalue.identifier.id,
                        instr.lvalue.identifier.name,
                        std::mem::discriminant(&instr.value)
                    );
                }
            }
            eprintln!("---END HIR---");
        }

        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---DO-WHILE OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
        assert!(result.code.contains("do {"), "should contain do-while loop");
    }

    #[test]
    fn test_jsx_member_expr() {
        let source = r#"function Component(props) {
  const maybeMutable = new MaybeMutable();
  return <Foo.Bar>{maybeMutate(maybeMutable)}</Foo.Bar>;
}"#;
        let options = options::PluginOptions::default();
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }

    #[test]
    fn test_array_map_lambda() {
        let source = r#"function Component(props) {
  const x = [];
  const y = x.map(item => {
    item.updated = true;
    return item;
  });
  return [x, y];
}"#;
        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }

    #[test]
    fn test_spread_jsx() {
        let source = r#"function Component() {
  const foo = () => {
    someGlobal = true;
  };
  return <div {...foo} />;
}"#;
        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }

    #[test]
    fn test_assignment_computed() {
        let source = r#"function Component(props) {
  const x = [props.x];
  const index = 0;
  x[index] *= 2;
  x['0'] += 3;
  return x;
}"#;
        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }

    #[test]
    fn test_let_const_promotion() {
        let source = r#"function Component(props) {
  let x = [];
  let y = [];
  x.push(props.a);
  y.push(props.b);
  return [x, y];
}"#;
        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
        // Variables that are never reassigned should be `const`, not `let`
        assert!(
            result.code.contains("const x = []"),
            "x should be const, got:\n{}",
            result.code
        );
        assert!(
            result.code.contains("const y = []"),
            "y should be const, got:\n{}",
            result.code
        );
    }

    /// Regression: useLayoutEffect callback captures updateStyles from a zero-dep
    /// sentinel scope. The callback scope should also be zero-dep (sentinel).
    #[test]
    fn test_repro_mutate_ref_in_function_passed_to_hook() {
        let source = r#"// @flow
component Example() {
  const fooRef = useRef();
  function updateStyles() {
    const foo = fooRef.current;
    if (barRef.current == null || foo == null) {
      return;
    }
    foo.style.height = '100px';
  }
  const barRef = useRef(null);
  const resizeRef = useResizeObserver(
    rect => {
      const {width} = rect;
      barRef.current = width;
    }
  );
  useLayoutEffect(() => {
    const observer = new ResizeObserver(_ => {
      updateStyles();
    });
    return () => {
      observer.disconnect();
    };
  }, []);
  return <div ref={resizeRef} />;
}"#;
        let result = compile("test.flow.js", source, &options::PluginOptions::default());
        assert!(result.transformed);
        assert!(
            !result.code.contains("!== updateStyles"),
            "useLayoutEffect callback should NOT depend on updateStyles"
        );
    }

    #[test]
    fn test_for_of() {
        let source = r#"function Component() {
  let x = [];
  let items = [0, 1, 2];
  for (const ii of items) {
    x.push(ii * 2);
  }
  return x;
}"#;
        let mut options = options::PluginOptions::default();
        options.compilation_mode = options::CompilationMode::All;
        let result = compile("test.js", source, &options);
        eprintln!("---FOR-OF OUTPUT---");
        eprintln!("{}", result.code);
        eprintln!("---END---");
        assert!(result.transformed);
    }
}
