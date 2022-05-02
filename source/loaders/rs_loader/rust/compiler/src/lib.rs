#![feature(rustc_private)]
#![feature(once_cell)]

extern crate rustc_ast;
extern crate rustc_ast_pretty;
extern crate rustc_attr;
extern crate rustc_driver;
extern crate rustc_error_codes;
extern crate rustc_errors;
extern crate rustc_feature;
extern crate rustc_hash;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_session;
extern crate rustc_span;

use dlopen;
use rustc_ast::{FloatTy, IntTy, UintTy};
use rustc_hir::MutTy;
use rustc_hir::{def::Res, FnRetTy, GenericArg, PrimTy, QPath, Ty, TyKind};
use rustc_interface::{interface::Compiler, Config, Queries};
use rustc_session::config;
use rustc_session::config::CrateType;
use rustc_span::source_map;
use std::fmt;
use std::{path::PathBuf, sync};
pub mod file;
pub mod memory;
pub mod package;
pub(crate) mod registrator;
pub mod wrapper;

use wrapper::generate_wrapper;

pub enum RegistrationError {
    CompilationError(String),
    DlopenError(String),
}

struct SourceInput(config::Input);

impl Clone for SourceInput {
    fn clone(&self) -> Self {
        match &self.0 {
            config::Input::File(path) => SourceInput(config::Input::File(path.clone())),
            config::Input::Str { name, input } => SourceInput(config::Input::Str {
                name: name.clone(),
                input: input.clone(),
            }),
        }
    }
}

pub struct SourceImpl {
    input: SourceInput,
    input_path: PathBuf,
    output: PathBuf,
}

pub enum Source {
    File { path: PathBuf },
    Memory { name: String, code: String },
}

impl Source {
    pub fn new(source: Source) -> SourceImpl {
        let library_name = |file_name: &PathBuf| {
            #[cfg(unix)]
            let lib_extension = "so";
            #[cfg(windows)]
            let lib_extension = "dll";
            #[cfg(macos)]
            let lib_extension = "dylib";

            let mut lib_name = file_name.clone();
            lib_name.set_extension(lib_extension);

            lib_name
        };

        let input_path = |dir: &PathBuf, name: &PathBuf| {
            let mut path = PathBuf::from(dir);
            path.push(name);
            path.clone()
        };

        let output_path = |dir: &PathBuf, name: &PathBuf| input_path(dir, &library_name(name));

        match source {
            Source::File { path } => {
                let dir = PathBuf::from(path.clone().parent().unwrap());
                let name = PathBuf::from(path.file_name().unwrap());

                SourceImpl {
                    input: SourceInput(config::Input::File(path.clone())),
                    input_path: input_path(&dir, &name),
                    output: output_path(&dir, &name),
                }
            }
            Source::Memory { name, code } => {
                let dir = PathBuf::from(std::env::temp_dir());
                let name_path = PathBuf::from(name.clone());

                SourceImpl {
                    input: SourceInput(config::Input::Str {
                        name: source_map::FileName::Custom(name.clone()),
                        input: code.clone(),
                    }),
                    input_path: input_path(&dir, &name_path),
                    output: output_path(&dir, &name_path),
                }
            }
        }
    }
}

fn toolchain_path(home: Option<String>, toolchain: Option<String>) -> Option<PathBuf> {
    home.and_then(|home| {
        toolchain.map(|toolchain| {
            let mut path = PathBuf::from(home);
            path.push("toolchains");
            path.push(toolchain);
            path
        })
    })
}

fn compiler_sys_root() -> Option<PathBuf> {
    // Get the sysroot, looking from most specific to this invocation to the least:
    // - runtime environment
    //    - SYSROOT
    //    - RUSTUP_HOME, MULTIRUST_HOME, RUSTUP_TOOLCHAIN, MULTIRUST_TOOLCHAIN
    // - sysroot from rustc in the path
    // - compile-time environment
    //    - SYSROOT
    //    - RUSTUP_HOME, MULTIRUST_HOME, RUSTUP_TOOLCHAIN, MULTIRUST_TOOLCHAIN
    std::env::var("SYSROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var("RUSTUP_HOME")
                .or_else(|_| std::env::var("MULTIRUST_HOME"))
                .ok();
            let toolchain = std::env::var("RUSTUP_TOOLCHAIN")
                .or_else(|_| std::env::var("MULTIRUST_TOOLCHAIN"))
                .ok();
            toolchain_path(home, toolchain)
        })
        .or_else(|| option_env!("SYSROOT").map(PathBuf::from))
        .or_else(|| {
            let home = option_env!("RUSTUP_HOME")
                .or(option_env!("MULTIRUST_HOME"))
                .map(ToString::to_string);
            let toolchain = option_env!("RUSTUP_TOOLCHAIN")
                .or(option_env!("MULTIRUST_TOOLCHAIN"))
                .map(ToString::to_string);
            toolchain_path(home, toolchain)
        })
}

fn compiler_source() -> Option<PathBuf> {
    match compiler_sys_root() {
        Some(sys_root) => {
            let mut path = sys_root.clone();
            path.push("lib");
            path.push("rustlib");
            path.push("src");
            path.push("rust");

            if path.exists() {
                return Some(path);
            } else {
                return None;
            }
        }
        _ => None,
    }
}

#[derive(Debug)]
pub struct DlopenLibrary {
    pub instance: dlopen::raw::Library,
}
impl DlopenLibrary {
    pub fn new(path_to_dll: &PathBuf) -> Result<DlopenLibrary, String> {
        match match dlopen::raw::Library::open(path_to_dll.clone()) {
            Ok(instance) => return Ok(DlopenLibrary { instance }),
            Err(error) => match error {
                dlopen::Error::NullCharacter(null_error) => {
                    Err(format!(
                        "Provided string could not be coverted into `{}` because it contained null character. IoError: {}",
                        "std::ffi::CString",
                        null_error
                    ))
                }
                dlopen::Error::OpeningLibraryError(io_error) => {
                    Err(format!(
                        "The dll could not be opened. IoError: {}",
                        io_error
                    ))
                }
                dlopen::Error::SymbolGettingError(io_error) => {
                    Err(format!(
                        "The symbol could not be obtained. IoError: {}",
                        io_error
                    ))
                }
                dlopen::Error::NullSymbol => {
                    Err(format!(
                        "Value of the symbol was null.",
                    ))
                }
                dlopen::Error::AddrNotMatchingDll(io_error) => {
                    Err(format!(
                        "Address could not be matched to a dynamic link library. IoError: {}",
                        io_error
                    ))
                }
            },
        } {
            Ok(dlopen_library_instance) => return Ok(dlopen_library_instance),
            Err(error) => {
                let dll_opening_error = format!(
                    "{}\nrs_loader was unable to open the dll with the following path: `{}`", 
                    error,
                    path_to_dll.to_str().unwrap()
                );

                return Err(dll_opening_error)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Mutability {
    Yes,
    No,
}
#[derive(Clone, Debug)]
pub enum Reference {
    Yes,
    No,
}

#[derive(Clone, Debug)]
pub enum FunctionType {
    I16,
    I32,
    I64,
    U16,
    U32,
    U64,
    Usize,
    F32,
    F64,
    Bool,
    Char,
    Array,
    Map,
    Slice,
    Str,
    String,
    Ptr,
    Null,
    Complex,
}

impl fmt::Display for FunctionType {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str(format!("{:?}", self).as_str())?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FunctionParameter {
    name: String,
    mutability: Mutability,
    reference: Reference,
    ty: FunctionType,
    generic: Vec<FunctionParameter>,
}

#[derive(Clone, Debug, Default)]
pub struct Function {
    name: String,
    ret: Option<FunctionParameter>,
    args: Vec<FunctionParameter>,
}

#[derive(Clone, Debug)]
pub struct CompilerState {
    output: PathBuf,
    functions: Vec<Function>,
}

#[derive(Clone, Debug)]
pub struct CompilerError {
    diagnostics: String,
    errors: String,
    err: String,
}

pub struct CompilerCallbacks {
    source: SourceImpl,
    functions: Vec<Function>,
}

fn handle_prim_ty(typ: PrimTy) -> FunctionType {
    match typ {
        PrimTy::Int(ty) => match ty {
            IntTy::I16 => return FunctionType::I16,
            IntTy::I32 => return FunctionType::I32,
            IntTy::I64 => return FunctionType::I64,
            _ => return FunctionType::Null,
        },
        PrimTy::Uint(ty) => match ty {
            UintTy::U16 => return FunctionType::U16,
            UintTy::U32 => return FunctionType::U32,
            UintTy::U64 => return FunctionType::U64,
            UintTy::Usize => return FunctionType::Usize,
            _ => return FunctionType::Null,
        },
        PrimTy::Float(ty) => match ty {
            FloatTy::F32 => return FunctionType::F32,
            FloatTy::F64 => return FunctionType::F64,
        },
        PrimTy::Bool => return FunctionType::Bool,
        PrimTy::Char => return FunctionType::Char,
        PrimTy::Str => return FunctionType::Str,
    }
}

fn handle_ty(ty: &Ty) -> FunctionParameter {
    let mut result = FunctionParameter {
        name: String::new(),
        mutability: Mutability::No,
        reference: Reference::No,
        ty: FunctionType::Null,
        generic: vec![],
    };
    match &ty.kind {
        TyKind::Path(path) => {
            if let QPath::Resolved(_, rpath) = path {
                match rpath.res {
                    Res::PrimTy(typ) => {
                        let segment = &rpath.segments[0];
                        result.name = segment.ident.name.to_string();
                        result.ty = handle_prim_ty(typ);
                    }
                    Res::Def(_, _) => {
                        let segment = &rpath.segments[0];
                        result.name = segment.ident.name.to_string();
                        if segment.ident.name.as_str() == "Vec" {
                            result.ty = FunctionType::Array;
                            // vec
                            if let Some(ga) = segment.args {
                                for arg in ga.args {
                                    match arg {
                                        GenericArg::Type(ty) => result.generic.push(handle_ty(ty)),
                                        _ => todo!(),
                                    }
                                }
                            }
                        } else if segment.ident.name.as_str() == "HashMap" {
                            result.ty = FunctionType::Map;
                            if let Some(ga) = segment.args {
                                for arg in ga.args {
                                    match arg {
                                        GenericArg::Type(ty) => result.generic.push(handle_ty(ty)),
                                        _ => todo!(),
                                    }
                                }
                            }
                        } else if segment.ident.name.as_str() == "String" {
                            result.ty = FunctionType::String;
                            dbg!(&result.ty);
                        }
                    }
                    _ => todo!(),
                }
            }
        }
        TyKind::Rptr(_, MutTy { ty, mutbl }) => {
            let mut inner_ty = handle_ty(ty);
            inner_ty.reference = Reference::Yes;
            match mutbl {
                rustc_hir::Mutability::Mut => inner_ty.mutability = Mutability::Yes,
                rustc_hir::Mutability::Not => inner_ty.mutability = Mutability::No,
            }
            return inner_ty;
        }
        _ => {}
    }
    result
}

impl rustc_driver::Callbacks for CompilerCallbacks {
    fn config(&mut self, config: &mut Config) {
        // Set up output
        config.output_file = Some(self.source.output.clone());
        // Set up inputs
        config.input = self.source.input.clone().0;
        config.input_path = Some(self.source.input_path.clone());

        // Setting up default compiler flags
        config.opts.output_types = config::OutputTypes::new(&[(config::OutputType::Exe, None)]);
        config.opts.optimize = config::OptLevel::Default;
        config.opts.unstable_features = rustc_feature::UnstableFeatures::Allow;
        config.opts.real_rust_source_base_dir = compiler_source();
        config.opts.edition = rustc_span::edition::Edition::Edition2021;
    }

    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &Compiler,
        queries: &'tcx Queries<'tcx>,
    ) -> rustc_driver::Compilation {
        // analysis
        if self.functions.len() == 0 {
            queries.global_ctxt().unwrap().peek_mut().enter(|tcx| {
                for item in tcx.hir().items() {
                    match &item.kind {
                        rustc_hir::ItemKind::Fn(sig, _, _) => {
                            let mut function = Function {
                                name: item.ident.to_string(),
                                ret: None,
                                args: vec![],
                            };
                            // parse input and output
                            for arg in sig.decl.inputs {
                                function.args.push(handle_ty(arg));
                            }

                            match sig.decl.output {
                                FnRetTy::DefaultReturn(_) => function.ret = None,
                                FnRetTy::Return(ty) => {
                                    function.ret = Some(handle_ty(ty));
                                }
                            }
                            self.functions.push(function);
                        }
                        _ => continue,
                    }
                }
            });

            return rustc_driver::Compilation::Stop;
        } else {
            // we have populated functions, continue
            return rustc_driver::Compilation::Continue;
        }
    }
}

// Buffer diagnostics in a Vec<u8>
#[derive(Clone)]
struct DiagnosticSink(sync::Arc<sync::Mutex<Vec<u8>>>);

impl std::io::Write for DiagnosticSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

const BUG_REPORT_URL: &str = "https://github.com/metacall/core/issues/new";

static ICE_HOOK: std::lazy::SyncLazy<
    Box<dyn Fn(&std::panic::PanicInfo<'_>) + Sync + Send + 'static>,
> = std::lazy::SyncLazy::new(|| {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| report_ice(info, BUG_REPORT_URL)));
    hook
});

fn report_ice(info: &std::panic::PanicInfo<'_>, bug_report_url: &str) {
    // Invoke our ICE handler, which prints the actual panic message and optionally a backtrace
    (*ICE_HOOK)(info);

    // Separate the output with an empty line
    eprintln!();

    let emitter = Box::new(rustc_errors::emitter::EmitterWriter::stderr(
        rustc_errors::ColorConfig::Auto,
        None,
        false,
        false,
        None,
        false,
    ));
    let handler = rustc_errors::Handler::with_emitter(true, None, emitter);

    // a .span_bug or .bug call has already printed what it wants to print
    if !info.payload().is::<rustc_errors::ExplicitBug>() {
        let d = rustc_errors::Diagnostic::new(rustc_errors::Level::Bug, "unexpected panic");
        handler.emit_diagnostic(&d);
    }

    let xs: Vec<std::borrow::Cow<'static, str>> = vec![
        "the compiler unexpectedly panicked. this is a bug.".into(),
        format!("we would appreciate a bug report: {}", bug_report_url).into(),
    ];

    for note in &xs {
        handler.note_without_error(note);
    }

    // If backtraces are enabled, also print the query stack
    let backtrace = std::env::var_os("RUST_BACKTRACE").map_or(false, |x| &x != "0");

    let num_frames = if backtrace { None } else { Some(2) };

    rustc_interface::interface::try_print_query_stack(&handler, num_frames);
}

pub fn initialize() {
    rustc_driver::init_rustc_env_logger();
    std::lazy::SyncLazy::force(&ICE_HOOK);
}

fn run_compiler(
    callbacks: &mut (dyn rustc_driver::Callbacks + Send),
    diagnostics_buffer: &sync::Arc<sync::Mutex<Vec<u8>>>,
    errors_buffer: &sync::Arc<sync::Mutex<Vec<u8>>>,
) -> Result<(), rustc_errors::ErrorReported> {
    let mut config = Config {
        // Command line options
        opts: config::Options {
            maybe_sysroot: compiler_sys_root(),
            crate_types: vec![CrateType::Cdylib],
            ..config::Options::default()
        },
        // cfg! configuration in addition to the default ones
        crate_cfg: rustc_hash::FxHashSet::default(), // FxHashSet<(String, Option<String>)>
        input: config::Input::File(PathBuf::new()),
        input_path: None,  // Option<PathBuf>
        output_dir: None,  // Option<PathBuf>
        output_file: None, // Option<PathBuf>
        file_loader: None, // Option<Box<dyn FileLoader + Send + Sync>>
        diagnostic_output: rustc_session::DiagnosticOutput::Raw(Box::from(DiagnosticSink(
            diagnostics_buffer.clone(),
        ))),
        // Set to capture stderr output during compiler execution
        stderr: Some(errors_buffer.clone()), // Option<Arc<Mutex<Vec<u8>>>>
        lint_caps: rustc_hash::FxHashMap::default(), // FxHashMap<lint::LintId, lint::Level>
        // This is a callback from the driver that is called when [`ParseSess`] is created.
        parse_sess_created: None, //Option<Box<dyn FnOnce(&mut ParseSess) + Send>>
        // This is a callback from the driver that is called when we're registering lints;
        // it is called during plugin registration when we have the LintStore in a non-shared state.
        //
        // Note that if you find a Some here you probably want to call that function in the new
        // function being registered.
        register_lints: None, // Option<Box<dyn Fn(&Session, &mut LintStore) + Send + Sync>>
        // This is a callback from the driver that is called just after we have populated
        // the list of queries.
        //
        // The second parameter is local providers and the third parameter is external providers.
        override_queries: None, // Option<fn(&Session, &mut ty::query::Providers<'_>, &mut ty::query::Providers<'_>)>
        // Registry of diagnostics codes.
        registry: rustc_errors::registry::Registry::new(&rustc_error_codes::DIAGNOSTICS),
        make_codegen_backend: None,
    };

    callbacks.config(&mut config);

    rustc_interface::run_compiler(config, |compiler| {
        let sess = compiler.session();

        let linker = compiler.enter(|queries| {
            let early_exit = || sess.compile_status().map(|_| None);

            queries.parse()?;

            if callbacks.after_parsing(compiler, queries) == rustc_driver::Compilation::Stop {
                return early_exit();
            }

            queries.expansion()?;
            if callbacks.after_expansion(compiler, queries) == rustc_driver::Compilation::Stop {
                return early_exit();
            }

            queries.prepare_outputs()?;

            queries.global_ctxt()?;

            queries
                .global_ctxt()?
                .peek_mut()
                .enter(|tcx| tcx.analysis(()))?;

            if callbacks.after_analysis(compiler, queries) == rustc_driver::Compilation::Stop {
                return early_exit();
            }

            queries.ongoing_codegen()?;

            let linker = queries.linker()?;
            Ok(Some(linker))
        })?;

        if let Some(linker) = linker {
            linker.link()?
        }

        Ok(())
    })
}

pub fn compile(source: SourceImpl) -> Result<CompilerState, CompilerError> {
    let mut callbacks = CompilerCallbacks {
        source,
        functions: vec![],
    };

    let diagnostics_buffer = sync::Arc::new(sync::Mutex::new(Vec::new()));
    let errors_buffer = sync::Arc::new(sync::Mutex::new(Vec::new()));

    // parse and generate wrapper
    let parsing_result: Result<(), CompilerError> = match rustc_driver::catch_fatal_errors(|| {
        run_compiler(&mut callbacks, &diagnostics_buffer, &errors_buffer)
    })
    .and_then(|result| result)
    {
        Ok(()) => Ok(()),
        Err(err) => {
            // Read buffered diagnostics
            let diagnostics =
                String::from_utf8(diagnostics_buffer.lock().unwrap().clone()).unwrap();
            eprintln!("{}", diagnostics);

            // Read buffered errors
            let errors = String::from_utf8(errors_buffer.lock().unwrap().clone()).unwrap();
            eprintln!("{}", errors);

            return Err(CompilerError {
                diagnostics,
                errors,
                err: format!("{:?}", err),
            });
        }
    };
    // parse fails, stop
    if let Err(e) = parsing_result {
        return Err(e);
    }

    let mut patched_callback = generate_wrapper(callbacks).unwrap();

    // generate binary
    match rustc_driver::catch_fatal_errors(|| {
        run_compiler(&mut patched_callback, &diagnostics_buffer, &errors_buffer)
    })
    .and_then(|result| result)
    {
        Ok(()) => Ok(CompilerState {
            output: patched_callback.source.output.clone(),
            functions: patched_callback.functions.clone(),
        }),
        Err(err) => {
            // Read buffered diagnostics
            let diagnostics =
                String::from_utf8(diagnostics_buffer.lock().unwrap().clone()).unwrap();
            eprintln!("{}", diagnostics);

            // Read buffered errors
            let errors = String::from_utf8(errors_buffer.lock().unwrap().clone()).unwrap();
            eprintln!("{}", errors);

            Err(CompilerError {
                diagnostics,
                errors,
                err: format!("{:?}", err),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static INIT: Once = Once::new();

    fn run_test<T>(test: T) -> ()
    where
        T: FnOnce() -> () + std::panic::UnwindSafe,
    {
        INIT.call_once(|| {
            // Initialize the compiler
            initialize();
        });
        let result = std::panic::catch_unwind(|| test());
        assert!(result.is_ok())
    }

    #[test]
    fn test_compile_memory() {
        run_test(|| {
            match compile(Source::new(Source::Memory {
                name: String::from("test.rs"),
                code: String::from(
                    "#[no_mangle]\npub extern \"C\" fn add(a: i32, b: i32) -> i32 { a + b }",
                ),
            })) {
                Err(comp_err) => assert!(false, "compilation failed: {}", comp_err.errors),
                Ok(comp_state) => assert!(comp_state.output.exists()),
            }
        })
    }

    #[test]
    fn test_compile_file() {
        run_test(|| {
            match compile(Source::new(Source::File {
                path: PathBuf::from(std::env::var("TEST_SOURCE_DIR").unwrap()),
            })) {
                Err(comp_err) => assert!(false, "compilation failed: {}", comp_err.errors),
                Ok(comp_state) => assert!(comp_state.output.exists()),
            }
        })
    }
}
