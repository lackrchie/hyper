//! # FFI 辅助宏模块
//!
//! 本模块定义了两个核心宏，用于简化 hyper FFI 层的函数导出与指针安全检查：
//!
//! - `ffi_fn!`：将 Rust 函数包装为 C ABI 兼容的导出函数，并提供 panic 安全保障。
//! - `non_null!`：对 FFI 传入的裸指针进行非空检查，避免空指针解引用导致的未定义行为。
//!
//! ## 在 hyper FFI 中的角色
//!
//! 这两个宏是整个 FFI 模块的基础设施，几乎所有导出到 C 的函数都依赖它们。
//! 它们共同确保了：
//! 1. Rust panic 不会跨越 FFI 边界传播（这是未定义行为）
//! 2. C 传入的空指针能被安全处理而不是导致段错误

/// `ffi_fn!` 宏 —— FFI 函数导出包装器
///
/// 此宏将普通 Rust 函数转换为可从 C 代码调用的 FFI 函数。它做了以下几件事：
///
/// 1. 添加 `#[no_mangle]` 属性：防止 Rust 编译器对函数名进行名称修饰（name mangling），
///    使 C 链接器能够通过原始函数名找到该函数。
/// 2. 添加 `pub extern "C"` 修饰：指定使用 C 调用约定（cdecl），确保参数传递和栈帧布局
///    与 C 编译器生成的代码兼容。
/// 3. 使用 `std::panic::catch_unwind` 包裹函数体：捕获 Rust panic，防止 panic
///    穿越 FFI 边界。panic 穿越 FFI 边界是未定义行为（UB），可能导致 C 端的栈损坏。
/// 4. 支持 `?= default_value` 语法：指定 panic 时的默认返回值。
///
/// ## 宏的四种变体
///
/// - 有返回值 + 有默认值：`fn name(args) -> Ret { body } ?= default`
/// - 有返回值 + 无默认值：`fn name(args) -> Ret { body }`（panic 时 abort 进程）
/// - 无返回值 + 有默认值：`fn name(args) { body } ?= default`
/// - 无返回值 + 无默认值：`fn name(args) { body }`（panic 时 abort 进程）
///
/// ## `AssertUnwindSafe` 的作用
///
/// `catch_unwind` 要求闭包实现 `UnwindSafe` trait。由于 FFI 函数通常会操作
/// 裸指针和可变引用，编译器无法自动推断 unwind 安全性。`AssertUnwindSafe`
/// 是对编译器的手动断言："我保证这个闭包在 panic 恢复后不会导致数据不一致"。
/// 在 FFI 场景下，panic 后直接返回错误码/空指针，不会再访问中间状态，因此是安全的。
macro_rules! ffi_fn {
    // 变体 1：有返回值 + 有 panic 默认值
    // `$doc` 捕获函数上方的文档注释属性（`///` 或 `#[doc = "..."]`）
    ($(#[$doc:meta])* fn $name:ident($($arg:ident: $arg_ty:ty),*) -> $ret:ty $body:block ?= $default:expr) => {
        $(#[$doc])*
        #[no_mangle] // 禁止名称修饰，使 C 链接器可以找到此符号
        pub extern "C" fn $name($($arg: $arg_ty),*) -> $ret {
            use std::panic::{self, AssertUnwindSafe};

            // catch_unwind 捕获所有 panic，防止其传播到 C 调用者
            match panic::catch_unwind(AssertUnwindSafe(move || $body)) {
                Ok(v) => v,       // 正常执行，返回结果
                Err(_) => {
                    $default      // panic 发生时返回调用者指定的安全默认值
                }
            }
        }
    };

    // 变体 2：有返回值 + 无 panic 默认值
    // 当没有合理的默认值时，panic 会导致进程直接终止（abort）。
    // 这比让 panic 穿越 FFI 边界更安全。
    ($(#[$doc:meta])* fn $name:ident($($arg:ident: $arg_ty:ty),*) -> $ret:ty $body:block) => {
        ffi_fn!($(#[$doc])* fn $name($($arg: $arg_ty),*) -> $ret $body ?= {
            eprintln!("panic unwind caught, aborting"); // 向标准错误输出诊断信息
            std::process::abort() // 立即终止进程，不执行析构函数
        });
    };

    // 变体 3：无返回值（返回 `()`）+ 有 panic 默认值
    // 委托给变体 1，显式指定返回类型为 `()`。
    ($(#[$doc:meta])* fn $name:ident($($arg:ident: $arg_ty:ty),*) $body:block ?= $default:expr) => {
        ffi_fn!($(#[$doc])* fn $name($($arg: $arg_ty),*) -> () $body ?= $default);
    };

    // 变体 4：无返回值 + 无 panic 默认值
    // 委托给变体 2。
    ($(#[$doc:meta])* fn $name:ident($($arg:ident: $arg_ty:ty),*) $body:block) => {
        ffi_fn!($(#[$doc])* fn $name($($arg: $arg_ty),*) -> () $body);
    };
}

/// `non_null!` 宏 —— FFI 指针非空检查
///
/// 在 FFI 边界上，C 调用者可能传入空指针（NULL）。直接解引用空指针是未定义行为（UB），
/// 会导致段错误。此宏提供了统一的非空检查模式：
///
/// 1. 在 debug 模式下使用 `debug_assert!` 断言指针非空（帮助开发时快速定位问题）
/// 2. 在 release 模式下进行运行时检查，空指针时提前返回错误值（优雅降级）
/// 3. 检查通过后执行 `unsafe` 操作（解引用、Box::from_raw 等）
///
/// ## 各变体说明
///
/// - `non_null!($ptr, $eval, $err)`：通用形式，检查 `$ptr` 非空后执行 `$eval`
/// - `non_null!(&*$ptr ?= $err)`：解引用为不可变引用
/// - `non_null!(&mut *$ptr ?= $err)`：解引用为可变引用
/// - `non_null!(Box::from_raw($ptr) ?= $err)`：将裸指针重新装箱（取回所有权）
/// - `non_null!(Arc::from_raw($ptr) ?= $err)`：将裸指针重建为 Arc（取回所有权）
///
/// `?= $err` 语法表示：如果指针为空，函数提前返回 `$err` 值。
macro_rules! non_null {
    // 通用形式：检查指针 $ptr 非空，然后在 unsafe 块中执行 $eval 表达式
    ($ptr:ident, $eval:expr, $err:expr) => {{
        // debug 模式下触发断言，方便开发阶段快速发现空指针 bug
        debug_assert!(!$ptr.is_null(), "{:?} must not be null", stringify!($ptr));
        if $ptr.is_null() {
            return $err; // release 模式下的运行时检查：空指针则提前返回错误值
        }
        unsafe { $eval } // 指针非空，安全地执行 unsafe 操作
    }};
    // 快捷形式：将裸指针解引用为不可变借用 `&T`
    (&*$ptr:ident ?= $err:expr) => {{
        non_null!($ptr, &*$ptr, $err)
    }};
    // 快捷形式：将裸指针解引用为可变借用 `&mut T`
    (&mut *$ptr:ident ?= $err:expr) => {{
        non_null!($ptr, &mut *$ptr, $err)
    }};
    // 快捷形式：通过 `Box::from_raw` 将裸指针转回 Box，重新获得所有权
    // 调用者随后可以 drop Box 来释放内存，或对其进行操作
    (Box::from_raw($ptr:ident) ?= $err:expr) => {{
        non_null!($ptr, Box::from_raw($ptr), $err)
    }};
    // 快捷形式：通过 `Arc::from_raw` 将裸指针转回 Arc，重新获得引用计数所有权
    (Arc::from_raw($ptr:ident) ?= $err:expr) => {{
        non_null!($ptr, Arc::from_raw($ptr), $err)
    }};
}
