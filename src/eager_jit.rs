//! Eager ORC compilation: all native symbols reach Ready before user entry.
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::NonNull;
use llvm_sys::prelude::{LLVMContextRef, LLVMModuleRef};
use llvm_sys::error::LLVMErrorRef;
use crate::codegen::{CodegenError, EagerProgram};

#[repr(C)]
struct Symbol { name: *const c_char, address: usize }
#[repr(C)]
struct Unit { name: *const c_char, token: u64 }
type Render = unsafe extern "C" fn(*mut c_void, u64, *mut LLVMModuleRef, *mut LLVMContextRef) -> LLVMErrorRef;
unsafe extern "C" {
    fn gcr_eager_create(symbols: *const Symbol, count: usize, units: *const Unit,
        unit_count: usize, render: Render, context: *mut c_void, threads: u32,
        result: *mut *mut c_void) -> *mut c_char;
    fn gcr_eager_lookup(jit: *mut c_void, name: *const c_char, address: *mut usize) -> *mut c_char;
    fn gcr_eager_dispose(jit: *mut c_void);
    fn gcr_eager_error_free(error: *mut c_char);
}

/// Native compilation workers for JIT and AOT builds.
pub(crate) fn compile_threads() -> Result<usize, CodegenError> {
    match std::env::var("GCR_COMPILE_THREADS") {
        Ok(value) => value.parse::<usize>().ok().filter(|&n| n > 0)
            .ok_or_else(|| CodegenError("GCR_COMPILE_THREADS must be a positive integer".into())),
        Err(std::env::VarError::NotPresent) => Ok(std::thread::available_parallelism().map_or(1, |n| n.get())),
        Err(_) => Err(CodegenError("GCR_COMPILE_THREADS is not UTF-8".into())),
    }
}

pub(crate) struct EagerJit {
    raw: NonNull<c_void>,
    // LLVM workers and engine are destroyed before their immutable render input.
    _program: Box<EagerProgram>,
}
impl EagerJit {
    pub(crate) fn new(program: Box<EagerProgram>, symbols: &[(&str, usize)]) -> Result<Self, CodegenError> {
        let partitions = program.symbols().iter().map(|(_, token)| *token).collect::<std::collections::BTreeSet<_>>().len();
        let threads = compile_threads()?.min(partitions.max(1)) as u32;
        let names: Vec<_> = symbols.iter().map(|(name, _)| CString::new(*name).unwrap()).collect();
        let symbols: Vec<_> = symbols.iter().zip(&names).map(|((_, address), name)|
            Symbol { name: name.as_ptr(), address: *address }).collect();
        let names: Vec<_> = program.symbols().iter().map(|(name, _)| CString::new(name.as_str()).unwrap()).collect();
        let units: Vec<_> = program.symbols().iter().zip(&names).map(|((_, token), name)|
            Unit { name: name.as_ptr(), token: *token }).collect();
        let mut raw = std::ptr::null_mut();
        let context = (&*program as *const EagerProgram).cast_mut().cast();
        unsafe { error(gcr_eager_create(symbols.as_ptr(), symbols.len(), units.as_ptr(), units.len(), render, context, threads, &mut raw)) }?;
        let raw = NonNull::new(raw).ok_or_else(|| CodegenError("LLVM returned a null eager JIT".into()))?;
        Ok(Self { raw, _program: program })
    }
    pub(crate) fn lookup(&self, name: &str) -> Result<usize, CodegenError> {
        let name = CString::new(name).map_err(|_| CodegenError("entry symbol contains NUL".into()))?;
        let mut address = 0;
        unsafe { error(gcr_eager_lookup(self.raw.as_ptr(), name.as_ptr(), &mut address)) }?;
        Ok(address)
    }
}
impl Drop for EagerJit {
    fn drop(&mut self) { unsafe { gcr_eager_dispose(self.raw.as_ptr()); } }
}
unsafe extern "C" fn render(context: *mut c_void, token: u64,
    module: *mut LLVMModuleRef, llvm_context: *mut LLVMContextRef) -> LLVMErrorRef {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        unsafe { &*context.cast::<EagerProgram>() }.render(token)
    }));
    match result {
        Ok(Ok((m, c))) => { unsafe { *module = m; *llvm_context = c; } std::ptr::null_mut() }
        result => {
            let message = match result {
                Ok(Err(error)) => error.0,
                Err(_) => "panic during eager JIT compilation".into(),
                _ => unreachable!(),
            };
            let message = CString::new(message).unwrap_or_else(|_| CString::new("invalid JIT diagnostic").unwrap());
            unsafe { llvm_sys::error::LLVMCreateStringError(message.as_ptr()) }
        }
    }
}
unsafe fn error(message: *mut c_char) -> Result<(), CodegenError> {
    if message.is_null() { return Ok(()); }
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned();
    unsafe { gcr_eager_error_free(message); }
    Err(CodegenError(text))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parallel_compilation_input_is_send_and_sync() {
        fn assert_shared<T: Send + Sync>() {}
        assert_shared::<crate::codegen::EagerProgram>();
    }
}

#[cfg(test)]
mod batch_tests {
    #[test]
    fn multiple_native_units_preserve_cross_unit_value_roots() {
        let functions = (0..100).map(|i| format!(r#"
            #[inline(never)] fn store{i}(mut a: Array<i64>, prefix: Prefix) {{
                let text = str_concat("x", "y");
                a[0] += prefix.code + str_len(prefix.name) + str_len(text);
            }}
        "#)).collect::<String>();
        let calls = (0..100).map(|i|format!("store{i}(a, prefix);")).collect::<String>();
        let source = format!(r#"
            #[value] struct Prefix {{ code: i64, name: String }}
            {functions}
            fn main() -> i64 {{
                let prefix = Prefix {{ code: 40, name: str_concat("r", "s") }};
                let a: Array<i64> = array_new(1);
                {calls}
                a[0]
            }}
        "#);
        let (module, _) = crate::compile::parse_with_prelude(&source).unwrap();
        let resolved = crate::resolve::resolve_module(module).unwrap();
        let program = crate::lower::lower_program(&resolved.globals).unwrap();
        let plan = crate::codegen::EagerProgram::new(&program).unwrap();
        let units: std::collections::BTreeSet<_> = plan.symbols().iter()
            .filter(|(_, token)| token >> 32 == 0).map(|(_, token)| *token).collect();
        assert!(units.len() > 2, "test must cross several native compilation units");
        assert!(units.len() < plan.symbols().len() / 2, "small functions should share native units");
        drop(plan);
        assert_eq!(unsafe { crate::codegen::jit_run_i64_gc(&program, true) }.unwrap(), 4400);
    }
}
