//! Stand-alone WebAssembly to Cretonne IL translator.
//!
//! This module defines the `FuncTranslator` type which can translate a single WebAssembly
//! function to Cretonne IL guided by a `FuncEnvironment` which provides information about the
//! WebAssembly module and the runtime environment.

use code_translator::translate_operator;
use cretonne::entity::EntityRef;
use cretonne::ir::{self, InstBuilder};
use cretonne::result::{CtonResult, CtonError};
use cton_frontend::{ILBuilder, FunctionBuilder};
use runtime::FuncEnvironment;
use state::TranslationState;
use translation_utils::Local;
use wasmparser::{self, BinaryReader};

/// Maximum number of local variables permitted in a function. The translation fails with a
/// `CtonError::ImplLimitExceeded` error if the limit is exceeded.
const MAX_LOCALS: usize = 50_000;

/// WebAssembly to Cretonne IL function translator.
///
/// A `FuncTranslator` is used to translate a binary WebAssembly function into Cretonne IL guided
/// by a `FuncEnvironment` object. A single translator instance can be reused to translate multiple
/// functions which will reduce heap allocation traffic.
pub struct FuncTranslator {
    il_builder: ILBuilder<Local>,
    state: TranslationState,
}

impl FuncTranslator {
    /// Create a new translator.
    pub fn new() -> FuncTranslator {
        FuncTranslator {
            il_builder: ILBuilder::new(),
            state: TranslationState::new(),
        }
    }

    /// Translate a binary WebAssembly function.
    ///
    /// The `code` slice contains the binary WebAssembly *function code* as it appears in the code
    /// section of a WebAssembly module, not including the initial size of the function code. The
    /// slice is expected to contain two parts:
    ///
    /// - The declaration of *locals*, and
    /// - The function *body* as an expression.
    ///
    /// See [the WebAssembly specification]
    /// (http://webassembly.github.io/spec/binary/modules.html#code-section).
    ///
    /// The Cretonne IR function `func` should be completely empty except for the `func.signature`
    /// and `func.name` fields. The signature may contain special-purpose arguments which are not
    /// regarded as WebAssembly local variables. Any signature arguments marked as
    /// `ArgumentPurpose::Normal` are made accessible as WebAssembly local variables.
    ///
    pub fn translate<FE: FuncEnvironment + ?Sized>(
        &mut self,
        code: &[u8],
        func: &mut ir::Function,
        environ: &mut FE,
    ) -> CtonResult {
        self.translate_from_reader(BinaryReader::new(code), func, environ)
    }

    /// Translate a binary WebAssembly function from a `BinaryReader`.
    pub fn translate_from_reader<FE: FuncEnvironment + ?Sized>(
        &mut self,
        mut reader: BinaryReader,
        func: &mut ir::Function,
        environ: &mut FE,
    ) -> CtonResult {
        dbg!(
            "translate({} bytes, {}{})",
            reader.bytes_remaining(),
            func.name,
            func.signature
        );
        assert_eq!(func.dfg.num_ebbs(), 0, "Function must be empty");
        assert_eq!(func.dfg.num_insts(), 0, "Function must be empty");

        // This clears the `ILBuilder`.
        let builder = &mut FunctionBuilder::new(func, &mut self.il_builder);
        let entry_block = builder.create_ebb();
        builder.switch_to_block(entry_block, &[]); // This also creates values for the arguments.
        builder.seal_block(entry_block);
        // Make sure the entry block is inserted in the layout before we make any callbacks to
        // `environ`. The callback functions may need to insert things in the entry block.
        builder.ensure_inserted_ebb();

        let num_args = declare_wasm_arguments(builder);

        // Set up the translation state with a single pushed control block representing the whole
        // function and its return values.
        let exit_block = builder.create_ebb();
        self.state.initialize(&builder.func.signature, exit_block);

        parse_local_decls(&mut reader, builder, num_args)?;
        parse_function_body(reader, builder, &mut self.state, environ)
    }
}

/// Declare local variables for the signature arguments that correspond to WebAssembly locals.
///
/// Return the number of local variables declared.
fn declare_wasm_arguments(builder: &mut FunctionBuilder<Local>) -> usize {
    let sig_len = builder.func.signature.argument_types.len();
    let mut next_local = 0;
    for i in 0..sig_len {
        let arg_type = builder.func.signature.argument_types[i];
        // There may be additional special-purpose arguments following the normal WebAssembly
        // signature arguments. For example, a `vmctx` pointer.
        if arg_type.purpose == ir::ArgumentPurpose::Normal {
            // This is a normal WebAssembly signature argument, so create a local for it.
            let local = Local::new(next_local);
            builder.declare_var(local, arg_type.value_type);
            next_local += 1;

            let arg_value = builder.arg_value(i);
            builder.def_var(local, arg_value);
        }
    }

    next_local
}

/// Parse the local variable declarations that precede the function body.
///
/// Declare local variables, starting from `num_args`.
fn parse_local_decls(
    reader: &mut BinaryReader,
    builder: &mut FunctionBuilder<Local>,
    num_args: usize,
) -> CtonResult {
    let mut next_local = num_args;
    let local_count = reader.read_local_count().map_err(
        |_| CtonError::InvalidInput,
    )?;

    let mut locals_total = 0;
    for _ in 0..local_count {
        let (count, ty) = reader.read_local_decl(&mut locals_total).map_err(|_| {
            CtonError::InvalidInput
        })?;
        declare_locals(builder, count, ty, &mut next_local)?;
    }

    Ok(())
}

/// Declare `count` local variables of the same type, starting from `next_local`.
///
/// Fail of too many locals are declared in the function, or if the type is not valid for a local.
fn declare_locals(
    builder: &mut FunctionBuilder<Local>,
    count: u32,
    wasm_type: wasmparser::Type,
    next_local: &mut usize,
) -> CtonResult {
    // All locals are initialized to 0.
    use wasmparser::Type::*;
    let zeroval = match wasm_type {
        I32 => builder.ins().iconst(ir::types::I32, 0),
        I64 => builder.ins().iconst(ir::types::I64, 0),
        F32 => builder.ins().f32const(ir::immediates::Ieee32::with_bits(0)),
        F64 => builder.ins().f64const(ir::immediates::Ieee64::with_bits(0)),
        _ => return Err(CtonError::InvalidInput),
    };

    let ty = builder.func.dfg.value_type(zeroval);
    for _ in 0..count {
        // This implementation limit is arbitrary, but it ensures that a small function can't blow
        // up the compiler by declaring millions of locals.
        if *next_local >= MAX_LOCALS {
            return Err(CtonError::ImplLimitExceeded);
        }

        let local = Local::new(*next_local);
        builder.declare_var(local, ty);
        builder.def_var(local, zeroval);
        *next_local += 1;
    }

    Ok(())
}

/// Parse the function body in `reader`.
///
/// This assumes that the local variable declarations have already been parsed and function
/// arguments and locals are declared in the builder.
fn parse_function_body<FE: FuncEnvironment + ?Sized>(
    mut reader: BinaryReader,
    builder: &mut FunctionBuilder<Local>,
    state: &mut TranslationState,
    environ: &mut FE,
) -> CtonResult {
    // The control stack is initialized with a single block representing the whole function.
    assert_eq!(state.control_stack.len(), 1, "State not initialized");

    // Keep going until the final `End` operator which pops the outermost block.
    while !state.control_stack.is_empty() {
        let op = reader.read_operator().map_err(|_| CtonError::InvalidInput)?;
        translate_operator(&op, builder, state, environ);
    }

    // The final `End` operator left us in the exit block where we need to manually add a return
    // instruction.
    //
    // If the exit block is unreachable, it may not have the correct arguments, so we would
    // generate a return instruction that doesn't match the signature.
    debug_assert!(builder.is_pristine());
    if !builder.is_unreachable() {
        builder.ins().return_(&state.stack);
    }

    debug_assert!(reader.eof());

    Ok(())
}

#[cfg(test)]
mod tests {
    use cretonne::{ir, Context};
    use cretonne::ir::types::I32;
    use runtime::{DummyRuntime, FuncEnvironment};
    use super::FuncTranslator;

    #[test]
    fn small1() {
        // Implicit return.
        //
        // (func $small1 (param i32) (result i32)
        //     (i32.add (get_local 0) (i32.const 1))
        // )
        const BODY: [u8; 7] = [
            0x00,       // local decl count
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 1
            0x6a,       // i32.add
            0x0b,       // end
        ];

        let mut trans = FuncTranslator::new();
        let mut runtime = DummyRuntime::default();
        let mut ctx = Context::new();

        ctx.func.name = ir::FunctionName::new("small1");
        ctx.func.signature.argument_types.push(
            ir::ArgumentType::new(I32),
        );
        ctx.func.signature.return_types.push(
            ir::ArgumentType::new(I32),
        );

        trans.translate(&BODY, &mut ctx.func, &mut runtime).unwrap();
        dbg!("{}", ctx.func.display(None));
        ctx.verify(runtime.flags()).unwrap();
    }

    #[test]
    fn small2() {
        // Same as above, but with an explicit return instruction.
        //
        // (func $small2 (param i32) (result i32)
        //     (return (i32.add (get_local 0) (i32.const 1)))
        // )
        const BODY: [u8; 8] = [
            0x00,       // local decl count
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 1
            0x6a,       // i32.add
            0x0f,       // return
            0x0b,       // end
        ];

        let mut trans = FuncTranslator::new();
        let mut runtime = DummyRuntime::default();
        let mut ctx = Context::new();

        ctx.func.name = ir::FunctionName::new("small2");
        ctx.func.signature.argument_types.push(
            ir::ArgumentType::new(I32),
        );
        ctx.func.signature.return_types.push(
            ir::ArgumentType::new(I32),
        );

        trans.translate(&BODY, &mut ctx.func, &mut runtime).unwrap();
        dbg!("{}", ctx.func.display(None));
        ctx.verify(runtime.flags()).unwrap();
    }

    #[test]
    fn infloop() {
        // An infinite loop, no return instructions.
        //
        // (func $infloop (result i32)
        //     (local i32)
        //     (loop (result i32)
        //         (i32.add (get_local 0) (i32.const 1))
        //         (set_local 0)
        //         (br 0)
        //     )
        // )
        const BODY: [u8; 16] = [
            0x01,       // 1 local decl.
            0x01, 0x7f, // 1 i32 local.
            0x03, 0x7f, // loop i32
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 0
            0x6a,       // i32.add
            0x21, 0x00, // set_local 0
            0x0c, 0x00, // br 0
            0x0b,       // end
            0x0b,       // end
        ];

        let mut trans = FuncTranslator::new();
        let mut runtime = DummyRuntime::default();
        let mut ctx = Context::new();

        ctx.func.name = ir::FunctionName::new("infloop");
        ctx.func.signature.return_types.push(
            ir::ArgumentType::new(I32),
        );

        trans.translate(&BODY, &mut ctx.func, &mut runtime).unwrap();
        dbg!("{}", ctx.func.display(None));
        ctx.verify(runtime.flags()).unwrap();
    }
}
