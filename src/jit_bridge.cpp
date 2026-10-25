// Eager, parallel LLVM O3 compilation. No lazy reexports or deferred
// code generation: the complete symbol set reaches Ready first.
#include "llvm/ExecutionEngine/Orc/LLJIT.h"
#include "llvm/ExecutionEngine/Orc/CompileUtils.h"
#include "llvm/ExecutionEngine/Orc/ExecutionUtils.h"
#include "llvm/IR/Verifier.h"
#include "llvm/IR/Function.h"
#include "llvm/Support/ModRef.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Support/CBindingWrapping.h"
#include "llvm/Support/TargetSelect.h"
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <map>
#include <condition_variable>
#if defined(__APPLE__)
#include <pthread/qos.h>
#endif
using namespace llvm;
using namespace llvm::orc;

struct GcrSymbol { const char *name; uintptr_t address; };
struct GcrUnit { const char *name; uint64_t token; };
using GcrRender = LLVMErrorRef (*)(void *, uint64_t, LLVMModuleRef *, LLVMContextRef *);
static char *failure(Error error) { return strdup(toString(std::move(error)).c_str()); }
extern "C" void gcr_eager_error_free(char *error) { free(error); }
static std::mutex trace_lock;
static void trace(StringRef phase, Module &module, std::chrono::steady_clock::time_point start) {
  std::lock_guard<std::mutex> lock(trace_lock);
  errs() << "gcr " << phase << ": " << module.getModuleIdentifier() << " "
      << std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count() << " ms\n";
}
class GcrCompiler final : public ConcurrentIRCompiler {
  bool timings;
public:
  GcrCompiler(JITTargetMachineBuilder configuration, bool timings)
      : ConcurrentIRCompiler(std::move(configuration)), timings(timings) {}
  Expected<std::unique_ptr<MemoryBuffer>> operator()(Module &module) override {
    auto start = std::chrono::steady_clock::now();
    auto result = ConcurrentIRCompiler::operator()(module);
    if (timings) trace("native", module, start);
    return result;
  }
};

class GcrFunctionUnit final : public MaterializationUnit {
  IRTransformLayer &layer;
  GcrRender render;
  void *context;
  uint64_t token;
public:
  GcrFunctionUnit(IRTransformLayer &layer, GcrRender render, void *context,
                  uint64_t token, SymbolFlagsMap symbols)
      : MaterializationUnit(Interface(std::move(symbols), nullptr)),
        layer(layer), render(render), context(context), token(token) {}
  StringRef getName() const override { return "gcr.eager.partition"; }
  void materialize(std::unique_ptr<MaterializationResponsibility> responsibility) override {
#if defined(__APPLE__)
    // These engine-owned workers compile an interactive foreground request.
    // Keep their work off the background/utility QoS inherited by launchers.
    pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
#endif
    LLVMModuleRef module = nullptr;
    LLVMContextRef llvm_context = nullptr;
    if (auto error = render(context, token, &module, &llvm_context)) {
      responsibility->getExecutionSession().reportError(unwrap(error));
      responsibility->failMaterialization();
      return;
    }
    auto owned_context = std::unique_ptr<LLVMContext>(unwrap(llvm_context));
    auto owned_module = std::unique_ptr<Module>(unwrap(module));
    layer.emit(std::move(responsibility), ThreadSafeModule(std::move(owned_module), std::move(owned_context)));
  }
  void discard(const JITDylib &, const SymbolStringPtr &) override {}
};

extern "C" char *gcr_eager_create(const GcrSymbol *symbols, size_t count,
    const GcrUnit *units, size_t unit_count, GcrRender render,
    void *context, unsigned threads, LLJIT **result) {
  *result = nullptr;
  InitializeNativeTarget();
  InitializeNativeTargetAsmPrinter();
  InitializeNativeTargetAsmParser();
  auto host = JITTargetMachineBuilder::detectHost();
  if (!host) return failure(host.takeError());
  host->setCodeGenOptLevel(CodeGenOptLevel::Aggressive);
  const char *timing_option = std::getenv("GCR_COMPILE_TIMINGS");
  const bool report = timing_option != nullptr;
  const bool timings = report && std::strcmp(timing_option, "summary") != 0;
  auto created = LLJITBuilder().setJITTargetMachineBuilder(*host)
      .setNumCompileThreads(threads)
      .setCompileFunctionCreator([timings](JITTargetMachineBuilder configuration) -> Expected<std::unique_ptr<IRCompileLayer::IRCompiler>> {
        std::unique_ptr<IRCompileLayer::IRCompiler> compiler = std::make_unique<GcrCompiler>(std::move(configuration), timings);
        return compiler;
      }).create();
  if (!created) return failure(created.takeError());
  auto jit = std::move(*created);
  auto &dylib = jit->getMainJITDylib();
  MangleAndInterner mangle(jit->getExecutionSession(), jit->getDataLayout());
  SymbolMap definitions;
  for (size_t i = 0; i < count; ++i)
    definitions[mangle(symbols[i].name)] = ExecutorSymbolDef(
        ExecutorAddr(symbols[i].address), JITSymbolFlags::Exported | JITSymbolFlags::Callable);
  if (auto error = dylib.define(absoluteSymbols(std::move(definitions)))) return failure(std::move(error));
  auto process = DynamicLibrarySearchGenerator::GetForCurrentProcess(jit->getDataLayout().getGlobalPrefix());
  if (!process) return failure(process.takeError());
  dylib.addGenerator(std::move(*process));
  jit->getIRTransformLayer().setTransform(
      [host = *host, timings](ThreadSafeModule tsm, MaterializationResponsibility &) -> Expected<ThreadSafeModule> {
        auto start = std::chrono::steady_clock::now();
        auto configuration = host;
        auto target = configuration.createTargetMachine();
        if (!target) return target.takeError();
        auto machine = std::move(*target);
        auto error = tsm.withModuleDo([&](Module &module) -> Error {
          LoopAnalysisManager loops;
          FunctionAnalysisManager functions;
          CGSCCAnalysisManager calls;
          ModuleAnalysisManager modules;
          PassBuilder builder(machine.get());
          builder.registerModuleAnalyses(modules);
          builder.registerCGSCCAnalyses(calls);
          builder.registerFunctionAnalyses(functions);
          builder.registerLoopAnalyses(loops);
          builder.crossRegisterProxies(loops, functions, calls, modules);
          ModulePassManager passes;
          if (auto error = builder.parsePassPipeline(passes,
              "function(sroa,instcombine<max-iterations=1;no-verify-fixpoint>),default<O3>"))
            return error;
          passes.run(module, modules);
          if (verifyModule(module, &errs()))
            return make_error<StringError>("optimized eager module verification failed", inconvertibleErrorCode());
          if (timings) trace("optimize", module, start);
          return Error::success();
        });
        if (error) return std::move(error);
        return std::move(tsm);
      });
  SymbolLookupSet complete;
  SymbolFlagsMap literals;
  std::map<uint64_t, SymbolFlagsMap> groups;
  for (size_t i = 0; i < unit_count; ++i) {
    auto symbol = mangle(units[i].name);
    complete.add(symbol);
    if (units[i].token >> 32 == 2) {
      literals[symbol] = JITSymbolFlags::Exported;
    } else {
      groups[units[i].token][symbol] = JITSymbolFlags::Exported | JITSymbolFlags::Callable;
    }
  }
  for (auto &group : groups) {
    if (auto error = dylib.define(std::make_unique<GcrFunctionUnit>(jit->getIRTransformLayer(),
        render, context, group.first, std::move(group.second)))) return failure(std::move(error));
  }
  if (!literals.empty()) {
    if (auto error = dylib.define(std::make_unique<GcrFunctionUnit>(jit->getIRTransformLayer(),
        render, context, 2ull << 32, std::move(literals)))) return failure(std::move(error));
  }
  // Schedule each materialization unit in the estimated-cost order supplied
  // by Rust. Keep every query's error, and join all Ready notifications before
  // the final all-symbol barrier. Nothing is deferred to a program call.
  struct Queries {
    std::mutex lock;
    std::condition_variable done;
    size_t pending;
    Error error = Error::success();
    explicit Queries(size_t pending) : pending(pending) {}
  };
  auto queries = std::make_shared<Queries>(unit_count);
  for (size_t i = 0; i < unit_count; ++i) {
    // Several symbols can share one unit. LLVM materializes the unit once,
    // while these queries retain each symbol's readiness and error state.
    SymbolLookupSet one;
    one.add(mangle(units[i].name));
    jit->getExecutionSession().lookup(LookupKind::Static,
        {{&dylib, JITDylibLookupFlags::MatchAllSymbols}}, std::move(one), SymbolState::Ready,
        [queries](Expected<SymbolMap> symbols) {
          std::lock_guard<std::mutex> lock(queries->lock);
          if (!symbols) queries->error = joinErrors(std::move(queries->error), symbols.takeError());
          --queries->pending;
          queries->done.notify_one();
        }, NoDependenciesToRegister);
  }
  {
    std::unique_lock<std::mutex> lock(queries->lock);
    queries->done.wait(lock, [&] { return queries->pending == 0; });
    if (queries->error) return failure(std::move(queries->error));
  }
  // Every exported function and callback must reach Ready before user entry.
  auto all = jit->getExecutionSession().lookup(
      {{&dylib, JITDylibLookupFlags::MatchAllSymbols}}, std::move(complete),
      LookupKind::Static, SymbolState::Ready);
  if (!all) return failure(all.takeError());
  if (report) errs() << "gcr eager compilation: " << unit_count
      << " symbols ready before entry; " << threads << " workers\n";
  *result = jit.release();
  return nullptr;
}
extern "C" char *gcr_eager_lookup(LLJIT *jit, const char *name, uintptr_t *address) {
  auto symbol = jit->lookup(name);
  if (!symbol) return failure(symbol.takeError());
  *address = symbol->getValue();
  return nullptr;
}
extern "C" void gcr_eager_dispose(LLJIT *jit) { delete jit; }

// Contracts of the legacy noncollecting String readers in runtime.rs. The
// versioned scanning ABIs may poll/relocate and must never receive these attrs.
extern "C" void gcr_llvm_readonly_leaf(LLVMValueRef value) {
  auto &function = *cast<Function>(unwrap(value));
  function.setMemoryEffects(MemoryEffects::argMemOnly(ModRefInfo::Ref));
  function.addFnAttr(Attribute::NoUnwind);
  function.addFnAttr(Attribute::WillReturn);
  function.addFnAttr(Attribute::NoSync);
  function.addFnAttr(Attribute::NoFree);
  for (auto &argument : function.args()) {
    if (!argument.getType()->isPointerTy()) continue;
    function.addParamAttr(argument.getArgNo(),
        Attribute::getWithCaptureInfo(function.getContext(), CaptureInfo::none()));
    function.addParamAttr(argument.getArgNo(), argument.getArgNo() == 0
        ? Attribute::ReadNone : Attribute::ReadOnly);
  }
}
