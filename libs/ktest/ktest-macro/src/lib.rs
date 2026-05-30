use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

/// 统一测试属性宏。
///
/// # 主机端（默认）
/// 展开为 `#[test]`，供 `cargo test` 使用。
///
/// # 内核端（feature = "kernel"）
/// 展开为 linker section 注册代码，供内核 test runner 遍历。
#[proc_macro_attribute]
pub fn ktest(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    if cfg!(feature = "kernel") {
        generate_kernel_entry(input)
    } else {
        generate_host_test(input)
    }
}

fn generate_host_test(input: ItemFn) -> TokenStream {
    let output = quote! {
        #[test]
        #input
    };
    output.into()
}

fn generate_kernel_entry(input: ItemFn) -> TokenStream {
    let fn_name = &input.sig.ident;
    let static_name = quote::format_ident!("__KTEST_{}", fn_name);
    let name_str = fn_name.to_string();

    let output = quote! {
        #input

        #[unsafe(link_section = ".ktest")]
        #[used]
        static #static_name: ktest::KtestEntry = ktest::KtestEntry {
            name: #name_str,
            file: file!(),
            line: line!(),
            func: || { #fn_name() },
        };
    };
    output.into()
}
