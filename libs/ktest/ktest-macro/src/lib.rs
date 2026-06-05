use proc_macro::TokenStream;
use quote::quote;
use syn::{Attribute, ItemFn, parse_macro_input};

/// 统一测试属性宏。
///
/// 同时生成两条路径，由调用方 rustc 根据 `cfg(test)` 选择：
///
/// - **主机端**（cargo test）：`#[cfg(test)]` + `#[test]` 展开为标准测试
/// - **内核端**（cargo build，test=false）：`#[cfg(not(test))]` 展开为
///   linker section 注册代码（`.ktest`），供内核 test runner 遍历
///
/// 内核二进制设置 `test = false`，因此 `cfg(test)` 永远不会在内核构建中
/// 成立，不存在 proc-macro `cfg!()` 的编译时求值歧义。
#[proc_macro_attribute]
pub fn ktest(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    let fn_name = &input.sig.ident;
    let host_name = quote::format_ident!("__ktest_host_{}", fn_name);
    let static_name = quote::format_ident!("__KTEST_{}", fn_name);

    // 收集需要传播给 host wrapper 的测试属性
    let test_attrs: Vec<&Attribute> = input
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("should_panic") || a.path().is_ident("ignore"))
        .collect();

    let output = quote! {
        #input

        // 主机端：cargo test
        #[cfg(test)]
        #( #test_attrs )*
        #[test]
        fn #host_name() { #fn_name(); }

        // 内核端：linker section 注册
        #[cfg(not(test))]
        #[allow(non_upper_case_globals)]
        #[unsafe(link_section = ".ktest")]
        #[used]
        static #static_name: ktest::KtestEntry = ktest::KtestEntry {
            name: concat!(module_path!(), "::", stringify!(#fn_name)),
            file: file!(),
            line: line!(),
            func: #fn_name,
        };
    };
    output.into()
}
