//! 内核直接符号目录的 attribute 实现。

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Expr, Ident, ImplItem, Item, ItemFn, ItemImpl, ItemStatic, LitInt, LitStr, Meta,
    ReturnType, Signature, Token, Type, Visibility, parse_macro_input,
};

struct ExportArgs {
    name: LitStr,
    contract: LitStr,
    version: u32,
    capabilities: Expr,
    flags: Option<Expr>,
    retained_args: Option<Expr>,
}

impl Parse for ExportArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut name = None;
        let mut contract = None;
        let mut version = None;
        let mut capabilities = None;
        let mut flags = None;
        let mut retained_args = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => assign_once(&mut name, input.parse()?, &key)?,
                "contract" => assign_once(&mut contract, input.parse()?, &key)?,
                "version" => {
                    let value: LitInt = input.parse()?;
                    assign_once(&mut version, value.base10_parse()?, &key)?;
                }
                "capabilities" => assign_once(&mut capabilities, input.parse()?, &key)?,
                "flags" => assign_once(&mut flags, input.parse()?, &key)?,
                "retained_args" => assign_once(&mut retained_args, input.parse()?, &key)?,
                _ => return Err(syn::Error::new_spanned(key, "未知内核符号导出参数")),
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        let args = Self {
            name: name.ok_or_else(|| syn::Error::new(Span::call_site(), "缺少 name"))?,
            contract: contract
                .ok_or_else(|| syn::Error::new(Span::call_site(), "缺少 contract"))?,
            version: version.ok_or_else(|| syn::Error::new(Span::call_site(), "缺少 version"))?,
            capabilities: capabilities
                .ok_or_else(|| syn::Error::new(Span::call_site(), "缺少 capabilities"))?,
            flags,
            retained_args,
        };
        validate_identifier(&args.name, "符号名称")?;
        validate_identifier(&args.contract, "符号契约")?;
        if args.version == 0 {
            return Err(syn::Error::new_spanned(
                &args.name,
                "内核符号版本必须大于零",
            ));
        }
        Ok(args)
    }
}

fn assign_once<T>(slot: &mut Option<T>, value: T, key: &Ident) -> syn::Result<()> {
    if slot.replace(value).is_some() {
        Err(syn::Error::new_spanned(key, "重复的内核符号导出参数"))
    } else {
        Ok(())
    }
}

/// 把常驻 Rust 函数或静态对象登记到内核直接符号目录。
#[proc_macro_attribute]
pub fn export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as Item);
    let attributes: TokenStream2 = attr.into();
    let result = match item {
        Item::Impl(item) => {
            if attributes.is_empty() {
                export_inherent_impl(item)
            } else {
                Err(syn::Error::new_spanned(
                    attributes,
                    "标记 impl 时，外层 kernel_symbols::export 不接受参数",
                ))
            }
        }
        item => syn::parse2::<ExportArgs>(attributes).and_then(|args| export_impl(args, item)),
    };
    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn export_impl(args: ExportArgs, item: Item) -> syn::Result<TokenStream2> {
    match item {
        Item::Fn(function) => export_function(args, function),
        Item::Static(item) => export_static(args, item),
        other => Err(syn::Error::new_spanned(
            other,
            "kernel_symbols::export 只能标记自由函数或静态对象",
        )),
    }
}

fn export_function(args: ExportArgs, mut function: ItemFn) -> syn::Result<TokenStream2> {
    let abi = canonical_function_abi(&function.sig)?;
    let ident = &function.sig.ident;
    let descriptor = descriptor_ident(ident);
    let name = args.name;
    let link_name = stable_link_name(&name);
    reject_explicit_linkage(&function.attrs)?;
    function
        .attrs
        .push(syn::parse_quote!(#[unsafe(export_name = #link_name)]));
    let contract = args.contract;
    let version = args.version;
    let capabilities = args.capabilities;
    let flags = args.flags.unwrap_or_else(|| syn::parse_quote!(0u32));
    let retained_args = args
        .retained_args
        .unwrap_or_else(|| syn::parse_quote!(0u64));
    let retention_flag = if is_zero_literal(&retained_args) {
        quote!(0u32)
    } else {
        quote!(::kernel_symbols::KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE)
    };
    let automatic_flags = if function.sig.unsafety.is_some() {
        quote!(::kernel_symbols::KERNEL_SYMBOL_FLAG_UNSAFE)
    } else {
        quote!(0u32)
    };
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[used]
        #[unsafe(link_section = ".elm.kernel_symbols")]
        static #descriptor: ::kernel_symbols::KernelSymbolDescriptorV1 =
            ::kernel_symbols::KernelSymbolDescriptorV1::function(
                #name,
                #contract,
                #version,
                #capabilities,
                (#flags) | #automatic_flags | #retention_flag,
                #retained_args,
                concat!(module_path!(), "::", stringify!(#ident)),
                #link_name,
                #abi,
                #ident as *const (),
            );
    })
}

fn export_static(args: ExportArgs, mut item: ItemStatic) -> syn::Result<TokenStream2> {
    if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
        return Err(syn::Error::new_spanned(
            &item,
            "不能直接导出 static mut；请提供经过审核的访问函数",
        ));
    }
    if args.retained_args.is_some() {
        return Err(syn::Error::new_spanned(
            &item,
            "静态对象不能声明 retained_args",
        ));
    }
    let ident = &item.ident;
    let descriptor = descriptor_ident(ident);
    let name = args.name;
    let link_name = stable_link_name(&name);
    reject_explicit_linkage(&item.attrs)?;
    item.attrs
        .push(syn::parse_quote!(#[unsafe(export_name = #link_name)]));
    let contract = args.contract;
    let version = args.version;
    let capabilities = args.capabilities;
    let flags = args.flags.unwrap_or_else(|| syn::parse_quote!(0u32));
    let ty = &item.ty;
    let abi = normalize_abi_tokens(quote!(static #ty));
    Ok(quote! {
        #item

        #[doc(hidden)]
        #[used]
        #[unsafe(link_section = ".elm.kernel_symbols")]
        static #descriptor: ::kernel_symbols::KernelSymbolDescriptorV1 =
            ::kernel_symbols::KernelSymbolDescriptorV1::static_object(
                #name,
                #contract,
                #version,
                #capabilities,
                #flags,
                concat!(module_path!(), "::", stringify!(#ident)),
                #link_name,
                #abi,
                ::core::ptr::addr_of!(#ident).cast::<()>(),
            );
    })
}

fn export_inherent_impl(mut item: ItemImpl) -> syn::Result<TokenStream2> {
    if item.trait_.is_some()
        || !item.generics.params.is_empty()
        || item.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            &item,
            "直接内核方法只能来自非泛型固有 impl",
        ));
    }
    let self_ty = (*item.self_ty).clone();
    let mut descriptors = Vec::new();
    for implementation_item in &mut item.items {
        let ImplItem::Fn(method) = implementation_item else {
            continue;
        };
        let Some((attribute_index, args)) = take_export_attribute(&method.attrs)? else {
            continue;
        };
        method.attrs.remove(attribute_index);
        if !matches!(method.vis, Visibility::Public(_)) {
            return Err(syn::Error::new_spanned(
                &method.sig,
                "导出的固有方法必须是 pub",
            ));
        }
        let abi = canonical_method_abi(&method.sig, &self_ty)?;
        let ident = &method.sig.ident;
        let descriptor = descriptor_ident_from_path(&args.name);
        let name = args.name;
        let link_name = stable_link_name(&name);
        reject_explicit_linkage(&method.attrs)?;
        method
            .attrs
            .push(syn::parse_quote!(#[unsafe(export_name = #link_name)]));
        let contract = args.contract;
        let version = args.version;
        let capabilities = args.capabilities;
        let flags = args.flags.unwrap_or_else(|| syn::parse_quote!(0u32));
        let retained_args = args
            .retained_args
            .unwrap_or_else(|| syn::parse_quote!(0u64));
        let retention_flag = if is_zero_literal(&retained_args) {
            quote!(0u32)
        } else {
            quote!(::kernel_symbols::KERNEL_SYMBOL_FLAG_RETAINS_MODULE_CODE)
        };
        let automatic_flags = if method.sig.unsafety.is_some() {
            quote!(::kernel_symbols::KERNEL_SYMBOL_FLAG_UNSAFE)
        } else {
            quote!(0u32)
        };
        descriptors.push(quote! {
            #[doc(hidden)]
            #[used]
            #[unsafe(link_section = ".elm.kernel_symbols")]
            static #descriptor: ::kernel_symbols::KernelSymbolDescriptorV1 =
                ::kernel_symbols::KernelSymbolDescriptorV1::method(
                    #name,
                    #contract,
                    #version,
                    #capabilities,
                    (#flags) | #automatic_flags | #retention_flag,
                    #retained_args,
                    concat!(
                        module_path!(),
                        "::",
                        stringify!(#self_ty),
                        "::",
                        stringify!(#ident)
                    ),
                    #link_name,
                    #abi,
                    <#self_ty>::#ident as *const (),
                );
        });
    }
    if descriptors.is_empty() {
        return Err(syn::Error::new_spanned(
            &item,
            "标记的 impl 中没有带 kernel_symbols::export 的方法",
        ));
    }
    Ok(quote! {
        #item
        #(#descriptors)*
    })
}

fn take_export_attribute(attributes: &[Attribute]) -> syn::Result<Option<(usize, ExportArgs)>> {
    for (index, attribute) in attributes.iter().enumerate() {
        let segments = &attribute.path().segments;
        let is_export = segments
            .last()
            .is_some_and(|segment| segment.ident == "export")
            && (segments.len() == 1
                || segments.len() == 2
                    && segments
                        .first()
                        .is_some_and(|segment| segment.ident == "kernel_symbols"));
        if !is_export {
            continue;
        }
        let Meta::List(list) = &attribute.meta else {
            return Err(syn::Error::new_spanned(
                attribute,
                "方法导出必须提供完整参数",
            ));
        };
        return syn::parse2::<ExportArgs>(list.tokens.clone()).map(|args| Some((index, args)));
    }
    Ok(None)
}

fn canonical_method_abi(signature: &Signature, self_ty: &Type) -> syn::Result<String> {
    if signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "直接内核方法必须是非泛型、非 async、非 const 的 Rust 方法",
        ));
    }
    let mut arguments = Vec::with_capacity(signature.inputs.len());
    for argument in &signature.inputs {
        match argument {
            syn::FnArg::Typed(argument) => {
                let ty = argument.ty.as_ref();
                arguments.push(quote!(#ty));
            }
            syn::FnArg::Receiver(receiver) => {
                if receiver.colon_token.is_some() {
                    let ty = receiver.ty.as_ref();
                    arguments.push(quote!(#ty));
                } else {
                    let mutability = &receiver.mutability;
                    if let Some((and_token, lifetime)) = &receiver.reference {
                        arguments.push(quote!(#and_token #lifetime #mutability #self_ty));
                    } else {
                        arguments.push(quote!(#self_ty));
                    }
                }
            }
        }
    }
    let unsafety = &signature.unsafety;
    let result: Type = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    let abi = normalize_abi_tokens(quote!(#unsafety fn(#(#arguments),*) -> #result));
    Ok(replace_self_type(
        &abi,
        &normalize_abi_tokens(quote!(#self_ty)),
    ))
}

fn replace_self_type(input: &str, self_type: &str) -> String {
    let mut output = String::with_capacity(input.len() + self_type.len());
    let bytes = input.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let is_self = bytes[index..].starts_with(b"Self")
            && (index == 0 || !is_rust_ident_byte(bytes[index - 1]))
            && (index + 4 == bytes.len() || !is_rust_ident_byte(bytes[index + 4]));
        if is_self {
            output.push_str(self_type);
            index += 4;
        } else {
            output.push(bytes[index] as char);
            index += 1;
        }
    }
    output
}

const fn is_rust_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn descriptor_ident_from_path(path: &LitStr) -> Ident {
    let mut suffix = String::with_capacity(path.value().len());
    for character in path.value().chars() {
        if character.is_ascii_alphanumeric() {
            suffix.push(character.to_ascii_uppercase());
        } else {
            suffix.push('_');
        }
    }
    format_ident!("__ELM_KERNEL_METHOD_DESCRIPTOR_{suffix}")
}

fn stable_link_name(path: &LitStr) -> LitStr {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.value().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    LitStr::new(&format!("__elm_kernel_api_{hash:016x}"), path.span())
}

fn reject_explicit_linkage(attributes: &[Attribute]) -> syn::Result<()> {
    if let Some(attribute) = attributes.iter().find(|attribute| {
        attribute.path().is_ident("no_mangle")
            || attribute.path().is_ident("export_name")
            || attribute.path().is_ident("link_name")
    }) {
        Err(syn::Error::new_spanned(
            attribute,
            "内核符号导出由框架统一生成链接名称，不能手工指定 linkage",
        ))
    } else {
        Ok(())
    }
}

fn is_zero_literal(expression: &Expr) -> bool {
    matches!(expression, Expr::Lit(literal) if matches!(&literal.lit, syn::Lit::Int(value) if value.base10_parse::<u64>().is_ok_and(|value| value == 0)))
}

fn descriptor_ident(item: &Ident) -> Ident {
    format_ident!(
        "__ELM_KERNEL_SYMBOL_DESCRIPTOR_{}",
        item.to_string().to_ascii_uppercase()
    )
}

fn canonical_function_abi(signature: &Signature) -> syn::Result<String> {
    if signature.constness.is_some()
        || signature.asyncness.is_some()
        || signature.abi.is_some()
        || signature.variadic.is_some()
        || !signature.generics.params.is_empty()
        || signature.generics.where_clause.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "直接内核符号必须是非泛型、非 async、非 const 的 Rust 函数",
        ));
    }
    let mut arguments = Vec::with_capacity(signature.inputs.len());
    for argument in &signature.inputs {
        let syn::FnArg::Typed(argument) = argument else {
            return Err(syn::Error::new_spanned(
                argument,
                "直接内核符号不能暴露 self 接收者；请导出自由函数 shim",
            ));
        };
        arguments.push(argument.ty.as_ref());
    }
    let unsafety = &signature.unsafety;
    let result: Type = match &signature.output {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, result) => (**result).clone(),
    };
    Ok(normalize_abi_tokens(
        quote!(#unsafety fn(#(#arguments),*) -> #result),
    ))
}

fn normalize_abi_tokens(tokens: TokenStream2) -> String {
    tokens
        .to_string()
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect()
}

fn validate_identifier(value: &LitStr, field: &str) -> syn::Result<()> {
    let value_text = value.value();
    if value_text.is_empty()
        || value_text.starts_with('.')
        || value_text.ends_with('.')
        || value_text.contains("..")
        || !value_text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'@'))
    {
        return Err(syn::Error::new_spanned(
            value,
            format!("{field} 不是规范 identifier"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_same_compact_rust_abi_shape() {
        let function: ItemFn = syn::parse_quote! {
            pub unsafe fn resize(pointer: *mut u8, old_size: usize) -> *mut u8 {
                pointer.wrapping_add(old_size)
            }
        };
        assert_eq!(
            canonical_function_abi(&function.sig).unwrap(),
            "unsafefn(*mutu8,usize)->*mutu8"
        );
    }
}
