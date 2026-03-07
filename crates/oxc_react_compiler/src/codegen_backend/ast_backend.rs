use crate::CompileResult;

use super::{CompiledFunction, ModuleEmitArgs};

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    super::raw::emit_module(args, compiled)
}
