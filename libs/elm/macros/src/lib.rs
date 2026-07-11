//! ELM Rust 开发属性宏。

use std::collections::BTreeMap;

use proc_macro::TokenStream;
use proc_macro2::{Ident, Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::{
    Expr, ExprLit, ExprUnary, Fields, FnArg, ItemFn, ItemStatic, ItemStruct, Lit, LitStr, Meta,
    Pat, ReturnType, Token, Type, UnOp, parse_macro_input,
};

const META_MAGIC: &[u8; 8] = b"ELMMETA1";
const META_VERSION: u16 = 1;
const META_HEADER_SIZE: usize = 32;

const KIND_LIFECYCLE: u16 = 1;
const KIND_ENTRY: u16 = 2;
const KIND_PROVIDER: u16 = 3;
const KIND_PROVIDER_SNAPSHOT: u16 = 4;
const KIND_EXPORT: u16 = 5;
const KIND_IMPORT: u16 = 6;
const KIND_EXTENSION_POINT: u16 = 7;
const KIND_EXTENSION: u16 = 8;
const KIND_PAYLOAD: u16 = 9;

const VALUE_UTF8: u16 = 1;
const VALUE_U32: u16 = 2;
const VALUE_I32: u16 = 3;

const FIELD_SYMBOL: u16 = 1;
const FIELD_HOOK_KIND: u16 = 2;
const FIELD_NAME: u16 = 3;
const FIELD_CONTRACT: u16 = 4;
const FIELD_MIN_VERSION: u16 = 5;
const FIELD_MAX_VERSION: u16 = 6;
const FIELD_VERSION: u16 = 7;
const FIELD_FLAGS: u16 = 8;
const FIELD_ACCESS: u16 = 9;
const FIELD_DIRECTION: u16 = 10;
const FIELD_MODE: u16 = 11;
const FIELD_TARGET: u16 = 12;
const FIELD_POINT: u16 = 13;
const FIELD_STAGE: u16 = 14;
const FIELD_PRIORITY: u16 = 15;
const FIELD_HANDLER_CONTRACT: u16 = 16;
const FIELD_PAYLOAD_CONTRACT: u16 = 17;
const FIELD_WIRE_SIZE: u16 = 18;

const IMPORT_OPTIONAL: u32 = 1 << 0;
const IMPORT_MANAGED: u32 = 1 << 1;
const IMPORT_DIRECT_PINNED: u32 = 1 << 2;
const IMPORT_ALLOW_ANCESTOR: u32 = 1 << 3;
const IMPORT_ALLOW_BUILTIN: u32 = 1 << 4;
const EXPORT_MANAGED: u32 = 1 << 0;
const EXPORT_DIRECT_PINNED: u32 = 1 << 1;
const EXPORT_PRIVATE: u32 = 1 << 2;
const EXPORT_DEPENDENCY: u32 = 1 << 3;
const EXPORT_SUBTREE: u32 = 1 << 4;
const EBI_NAME_LEN: usize = 128;
const EBI_SYMBOL_NAME_LEN: usize = 128;
const NEXUS_CONTRACT_LEN: usize = 64;
const RELATION_POINT_LEN: usize = 32;

#[proc_macro_attribute]
pub fn on_initialize(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 1, 1, "on_initialize")
}

#[proc_macro_attribute]
pub fn on_finalize(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 2, 2, "on_finalize")
}

#[proc_macro_attribute]
pub fn on_quiesce(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 6, 3, "on_quiesce")
}

#[proc_macro_attribute]
pub fn on_pause(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 7, 4, "on_pause")
}

#[proc_macro_attribute]
pub fn on_resume(attr: TokenStream, item: TokenStream) -> TokenStream {
    lifecycle_attribute(attr, item, 8, 5, "on_resume")
}

#[proc_macro_attribute]
pub fn on_migrate_export(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_export_attribute(attr, item)
}

#[proc_macro_attribute]
pub fn on_migrate_import(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_input_attribute(attr, item, 4, 7, "on_migrate_import")
}

#[proc_macro_attribute]
pub fn on_migrate_abort(attr: TokenStream, item: TokenStream) -> TokenStream {
    migration_input_attribute(attr, item, 5, 8, "on_migrate_abort")
}

#[proc_macro_attribute]
pub fn entry(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match entry_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn provider(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match provider_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn provider_snapshot(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match provider_snapshot_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn export(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match export_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn import(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStatic);
    match import_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn payload(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemStruct);
    match payload_impl(attr.into(), item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn mixin_point(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match mixin_point_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[proc_macro_attribute]
pub fn mixin(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match mixin_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn lifecycle_attribute(
    attr: TokenStream,
    item: TokenStream,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match lifecycle_impl(attr.into(), function, hook_kind, phase, symbol) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn lifecycle_impl(
    attr: TokenStream2,
    function: ItemFn,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 1)?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_{}", symbol);
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, hook_kind),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeHookContextV1,
        ) -> i32 {
            unsafe {
                ::elm::developer::__private::lifecycle_trampoline(context, #phase, #ident)
            }
        }

        #metadata
    })
}

fn migration_export_attribute(attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match migration_export_impl(attr.into(), function) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn migration_export_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 2)?;
    let ident = &function.sig.ident;
    let symbol = "on_migrate_export";
    let abi_ident = format_ident!("__elm_abi_on_migrate_export");
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, 3),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::developer::__private::migration_export_trampoline(context, #ident)
            }
        }

        #metadata
    })
}

fn migration_input_attribute(
    attr: TokenStream,
    item: TokenStream,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    match migration_input_impl(attr.into(), function, hook_kind, phase, symbol) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn migration_input_impl(
    attr: TokenStream2,
    function: ItemFn,
    hook_kind: u32,
    phase: u16,
    symbol: &str,
) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 2)?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_{}", symbol);
    let metadata = metadata_item(
        ident,
        symbol,
        metadata_record(
            KIND_LIFECYCLE,
            vec![
                MetaField::utf8(FIELD_SYMBOL, symbol),
                MetaField::u32(FIELD_HOOK_KIND, hook_kind),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            context: *mut ::elm::ElmNativeMigrationContextV1,
        ) -> i32 {
            unsafe {
                ::elm::developer::__private::migration_input_trampoline(
                    context,
                    #phase,
                    #ident,
                )
            }
        }

        #metadata
    })
}

fn entry_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    require_empty_attr(attr)?;
    validate_function(&function, 1)?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_entry_{}", ident);
    validate_symbol(&symbol, "entry symbol")?;
    let abi_ident = format_ident!("__elm_abi_entry_{}", ident);
    let metadata = metadata_item(
        ident,
        "entry",
        metadata_record(KIND_ENTRY, vec![MetaField::utf8(FIELD_SYMBOL, &symbol)]),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeEntryFrameV1,
        ) -> i32 {
            unsafe { ::elm::developer::__private::entry_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn provider_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let contract = args.required_string("contract")?;
    validate_contract(&contract)?;
    let access = parse_access(args.string_or("access", "public")?)?;
    let direction = parse_direction(args.string_or("direction", "control")?)?;
    let mode = parse_mode(args.string_or("mode", "shared")?)?;
    args.finish()?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_provider_{}", ident);
    validate_symbol(&symbol, "provider symbol")?;
    let abi_ident = format_ident!("__elm_abi_provider_{}", ident);
    let metadata = metadata_item(
        ident,
        "provider",
        metadata_record(
            KIND_PROVIDER,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_FLAGS, 0),
                MetaField::u32(FIELD_ACCESS, access),
                MetaField::u32(FIELD_DIRECTION, direction),
                MetaField::u32(FIELD_MODE, mode),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderCallV1,
        ) -> i32 {
            unsafe { ::elm::developer::__private::provider_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn provider_snapshot_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 2)?;
    let args = MetaArgs::parse(attr)?;
    let contract = args.required_string("contract")?;
    validate_contract(&contract)?;
    args.finish()?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_provider_snapshot_{}", ident);
    validate_symbol(&symbol, "provider snapshot symbol")?;
    let abi_ident = format_ident!("__elm_abi_provider_snapshot_{}", ident);
    let metadata = metadata_item(
        ident,
        "provider_snapshot",
        metadata_record(
            KIND_PROVIDER_SNAPSHOT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &contract),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderSnapshotV1,
        ) -> i32 {
            unsafe { ::elm::developer::__private::snapshot_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn export_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let name = args.string_or("name", &function.sig.ident.to_string())?;
    let contract = args.required_string("contract")?;
    let version = args.required_u32("version")?;
    validate_symbol(&name, "export name")?;
    validate_contract(&contract)?;
    if version == 0 {
        return Err(syn::Error::new(
            Span::call_site(),
            "export version 必须大于 0",
        ));
    }
    let mode = args.string_or("mode", "managed")?;
    let visibility = args.string_or("visibility", "dependency")?;
    let mut flags = match mode.as_str() {
        "managed" => EXPORT_MANAGED,
        "direct-pinned" => EXPORT_DIRECT_PINNED,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 export mode")),
    };
    flags |= match visibility.as_str() {
        "dependency" => EXPORT_DEPENDENCY,
        "private" => EXPORT_PRIVATE,
        "subtree" => EXPORT_SUBTREE,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 export visibility")),
    };
    args.finish()?;
    let ident = &function.sig.ident;
    let abi_ident = format_ident!("__elm_abi_export_{}", ident);
    let metadata = metadata_item(
        ident,
        "export",
        metadata_record(
            KIND_EXPORT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &name),
                MetaField::utf8(FIELD_NAME, &name),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_VERSION, version),
                MetaField::u32(FIELD_FLAGS, flags),
            ],
        ),
    );
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #name)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeManagedCallV1,
        ) -> i32 {
            unsafe { ::elm::developer::__private::managed_trampoline(frame, #ident) }
        }

        #metadata
    })
}

fn import_impl(attr: TokenStream2, mut item: ItemStatic) -> syn::Result<TokenStream2> {
    let args = MetaArgs::parse(attr)?;
    let name = args.required_string("name")?;
    let contract = args.required_string("contract")?;
    let min_version = args.u32_or("min_version", args.u32_or("version", 1)?)?;
    let max_version = args.u32_or("max_version", min_version)?;
    validate_symbol(&name, "import name")?;
    validate_contract(&contract)?;
    if min_version == 0 || max_version < min_version {
        return Err(syn::Error::new(
            Span::call_site(),
            "import 版本范围必须满足 1 <= min_version <= max_version",
        ));
    }
    let mode = args.string_or("mode", "managed")?;
    validate_import_slot(&item, &mode)?;
    let scope = args.string_or("scope", "any")?;
    let optional = args.bool_or("optional", false)?;
    let mut flags = match mode.as_str() {
        "managed" => IMPORT_MANAGED,
        "direct-pinned" => IMPORT_DIRECT_PINNED,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import mode")),
    };
    if optional {
        flags |= IMPORT_OPTIONAL;
    }
    flags |= match scope.as_str() {
        "any" => 0,
        "ancestor" => IMPORT_ALLOW_ANCESTOR,
        "builtin" => IMPORT_ALLOW_BUILTIN,
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import scope")),
    };
    args.finish()?;
    let ident = item.ident.clone();
    let symbol = format!("__elm_import_{}", ident.to_string().to_ascii_lowercase());
    validate_symbol(&symbol, "import slot symbol")?;
    item.attrs.push(syn::parse_quote!(#[used]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(export_name = #symbol)]));
    item.attrs
        .push(syn::parse_quote!(#[unsafe(link_section = ".data.elm_imports")]));
    let metadata = metadata_item(
        &ident,
        "import",
        metadata_record(
            KIND_IMPORT,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_NAME, &name),
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::u32(FIELD_MIN_VERSION, min_version),
                MetaField::u32(FIELD_MAX_VERSION, max_version),
                MetaField::u32(FIELD_FLAGS, flags),
            ],
        ),
    );
    Ok(quote! {
        #item
        #metadata
    })
}

fn payload_impl(attr: TokenStream2, item: ItemStruct) -> syn::Result<TokenStream2> {
    let contract = syn::parse2::<LitStr>(attr)?.value();
    validate_contract(&contract)?;
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &item.generics,
            "ELM 固定载荷不允许泛型参数",
        ));
    }
    let Fields::Named(fields) = &item.fields else {
        return Err(syn::Error::new_spanned(
            &item.fields,
            "ELM 固定载荷必须使用具名字段结构体",
        ));
    };
    let mut wire_size = 0usize;
    let mut encoders = Vec::new();
    let mut decoders = Vec::new();
    for field in &fields.named {
        let ident = field.ident.as_ref().expect("具名字段");
        let wire = WireType::parse(&field.ty)?;
        wire_size = wire_size
            .checked_add(wire.size())
            .ok_or_else(|| syn::Error::new_spanned(&field.ty, "载荷尺寸溢出"))?;
        encoders.push(wire.encoder(ident));
        decoders.push(wire.decoder(ident));
    }
    if wire_size > 256 {
        return Err(syn::Error::new_spanned(
            &item.ident,
            "ELM v1 固定载荷不得超过 256 字节",
        ));
    }
    let ident = &item.ident;
    let metadata = metadata_item(
        ident,
        "payload",
        metadata_record(
            KIND_PAYLOAD,
            vec![
                MetaField::utf8(FIELD_PAYLOAD_CONTRACT, &contract),
                MetaField::u32(FIELD_WIRE_SIZE, wire_size as u32),
            ],
        ),
    );
    Ok(quote! {
        #item

        impl ::elm::ElmPayload for #ident {
            const CONTRACT: &'static str = #contract;
            const WIRE_SIZE: usize = #wire_size;

            fn encode(
                &self,
                output: &mut [u8],
            ) -> ::core::result::Result<usize, ::elm::PayloadError> {
                if output.len() < Self::WIRE_SIZE {
                    return Err(::elm::PayloadError::BufferTooSmall);
                }
                let mut offset = 0usize;
                #(#encoders)*
                Ok(offset)
            }

            fn decode(
                input: &[u8],
            ) -> ::core::result::Result<Self, ::elm::PayloadError> {
                if input.len() != Self::WIRE_SIZE {
                    return Err(::elm::PayloadError::SizeMismatch);
                }
                let mut offset = 0usize;
                let value = Self {
                    #(#decoders),*
                };
                if offset != input.len() {
                    return Err(::elm::PayloadError::SizeMismatch);
                }
                Ok(value)
            }
        }

        #metadata
    })
}

fn mixin_point_impl(attr: TokenStream2, mut function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let point = args.string_or("name", &function.sig.ident.to_string())?;
    let contract = args.required_string("contract")?;
    let stages = args.stages()?;
    args.finish()?;
    validate_contract(&contract)?;
    for (stage, bit) in [
        ("ingress", 1),
        ("substitute", 2),
        ("egress", 4),
        ("observe", 8),
    ] {
        if stages & bit != 0 {
            validate_point(&format!("{point}.{stage}"))?;
        }
    }
    let (argument, _) = mutable_reference_argument(&function)?;
    let original_ident = format_ident!("__elm_original_{}", function.sig.ident);
    let wrapper_ident = function.sig.ident.clone();
    let visibility = function.vis.clone();
    let signature = function.sig.clone();
    let wrapper_attrs = function.attrs.clone();
    function.sig.ident = original_ident.clone();
    function.vis = syn::Visibility::Inherited;
    function.attrs.clear();

    let mut records = Vec::new();
    let ingress = stage_point(
        &point,
        "ingress",
        stages & 1 != 0,
        1,
        1,
        &contract,
        &mut records,
    );
    let substitute = stage_point(
        &point,
        "substitute",
        stages & 2 != 0,
        2,
        3,
        &contract,
        &mut records,
    );
    let egress = stage_point(
        &point,
        "egress",
        stages & 4 != 0,
        3,
        1,
        &contract,
        &mut records,
    );
    let observe = stage_point(
        &point,
        "observe",
        stages & 8 != 0,
        4,
        2,
        &contract,
        &mut records,
    );
    let metadata = metadata_item(&wrapper_ident, "mixin_point", metadata_blob(records));
    Ok(quote! {
        #function

        #(#wrapper_attrs)*
        #visibility #signature {
            ::elm::run_mixin_point(
                ::elm::MixinPointDescriptor {
                    contract: #contract,
                    ingress: #ingress,
                    substitute: #substitute,
                    egress: #egress,
                    observe: #observe,
                },
                #argument,
                #original_ident,
            )
        }

        #metadata
    })
}

fn mixin_impl(attr: TokenStream2, function: ItemFn) -> syn::Result<TokenStream2> {
    validate_function(&function, 1)?;
    let args = MetaArgs::parse(attr)?;
    let target = args.required_string("target")?;
    let point = args.required_string("point")?;
    let stage = args.required_string("stage")?;
    let contract = args.required_string("contract")?;
    let priority = args.i32_or("priority", 0)?;
    let default_handler_contract = format!("elm.mixin.{}@1", function.sig.ident);
    let handler_contract = args.string_or("handler_contract", &default_handler_contract)?;
    args.finish()?;
    validate_identifier(&target, EBI_NAME_LEN, "mixin target")?;
    validate_contract(&contract)?;
    validate_contract(&handler_contract)?;
    let (_, frame_ty) = mutable_reference_argument(&function)?;
    let stage_code = parse_stage(&stage)?;
    let full_point = format!("{point}.{stage}");
    validate_point(&full_point)?;
    let ident = &function.sig.ident;
    let symbol = format!("__elm_mixin_{}", ident);
    validate_symbol(&symbol, "mixin symbol")?;
    let abi_ident = format_ident!("__elm_abi_mixin_{}", ident);
    let records = vec![
        metadata_record(
            KIND_PROVIDER,
            vec![
                MetaField::utf8(FIELD_SYMBOL, &symbol),
                MetaField::utf8(FIELD_CONTRACT, &handler_contract),
                MetaField::u32(FIELD_FLAGS, 0),
                MetaField::u32(FIELD_ACCESS, 3),
                MetaField::u32(FIELD_DIRECTION, 4),
                MetaField::u32(FIELD_MODE, 2),
            ],
        ),
        metadata_record(
            KIND_EXTENSION,
            vec![
                MetaField::utf8(FIELD_CONTRACT, &contract),
                MetaField::utf8(FIELD_TARGET, &target),
                MetaField::utf8(FIELD_POINT, &full_point),
                MetaField::u32(FIELD_STAGE, stage_code),
                MetaField::i32(FIELD_PRIORITY, priority),
                MetaField::utf8(FIELD_HANDLER_CONTRACT, &handler_contract),
                MetaField::utf8(FIELD_PAYLOAD_CONTRACT, &contract),
            ],
        ),
    ];
    let metadata = metadata_item(ident, "mixin", metadata_blob(records));
    Ok(quote! {
        #function

        #[doc(hidden)]
        #[unsafe(export_name = #symbol)]
        #[unsafe(link_section = ".text.elm.abi")]
        pub unsafe extern "C" fn #abi_ident(
            frame: *mut ::elm::ElmNativeProviderCallV1,
        ) -> i32 {
            unsafe {
                ::elm::developer::__private::mixin_trampoline::<#frame_ty>(frame, #ident)
            }
        }

        #metadata
    })
}

fn stage_point(
    point: &str,
    stage: &str,
    enabled: bool,
    stage_code: u32,
    mode: u32,
    contract: &str,
    records: &mut Vec<Vec<u8>>,
) -> TokenStream2 {
    if !enabled {
        return quote!(None);
    }
    let full = format!("{point}.{stage}");
    records.push(metadata_record(
        KIND_EXTENSION_POINT,
        vec![
            MetaField::utf8(FIELD_CONTRACT, contract),
            MetaField::u32(FIELD_MODE, mode),
            MetaField::utf8(FIELD_POINT, &full),
            MetaField::u32(FIELD_STAGE, stage_code),
            MetaField::utf8(FIELD_PAYLOAD_CONTRACT, contract),
        ],
    ));
    quote!(Some(#full))
}

fn parse_stage(stage: &str) -> syn::Result<u32> {
    match stage {
        "ingress" => Ok(1),
        "substitute" => Ok(2),
        "egress" => Ok(3),
        "observe" => Ok(4),
        _ => Err(syn::Error::new(
            Span::call_site(),
            "mixin stage 必须是 ingress、substitute、egress 或 observe",
        )),
    }
}

fn parse_access(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "internal" => Ok(1),
        "public" => Ok(2),
        "extension-only" => Ok(3),
        _ => Err(syn::Error::new(Span::call_site(), "未知 provider access")),
    }
}

fn parse_direction(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "source" => Ok(1),
        "sink" => Ok(2),
        "duplex" => Ok(3),
        "control" => Ok(4),
        _ => Err(syn::Error::new(
            Span::call_site(),
            "未知 provider direction",
        )),
    }
}

fn parse_mode(value: String) -> syn::Result<u32> {
    match value.as_str() {
        "exclusive" => Ok(1),
        "shared" => Ok(2),
        "ordered" => Ok(3),
        "pipeline" => Ok(4),
        "broadcast" => Ok(5),
        _ => Err(syn::Error::new(Span::call_site(), "未知 provider mode")),
    }
}

fn validate_identifier(value: &str, max_len: usize, label: &str) -> syn::Result<()> {
    if value.is_empty()
        || value.len() > max_len
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            format!("{label} 不是有效 identifier"),
        ));
    }
    Ok(())
}

fn validate_symbol(value: &str, label: &str) -> syn::Result<()> {
    if value.is_empty()
        || value.len() > EBI_SYMBOL_NAME_LEN
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            format!("{label} 不是有效 EBI symbol"),
        ));
    }
    Ok(())
}

fn validate_contract(value: &str) -> syn::Result<()> {
    let Some((name, version)) = value.rsplit_once('@') else {
        return Err(syn::Error::new(
            Span::call_site(),
            "contract 必须包含 @version",
        ));
    };
    if value.len() > NEXUS_CONTRACT_LEN
        || name.is_empty()
        || version.is_empty()
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
        || !version.split('.').all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(syn::Error::new(
            Span::call_site(),
            "contract 不是有效的 ELM 契约 identifier",
        ));
    }
    Ok(())
}

fn validate_import_slot(item: &ItemStatic, mode: &str) -> syn::Result<()> {
    if matches!(item.mutability, syn::StaticMutability::Mut(_)) {
        return Err(syn::Error::new_spanned(
            &item.mutability,
            "ELM import 槽必须是不可变 static，内部写入由框架 UnsafeCell 承担",
        ));
    }
    if let Some(attribute) = item.attrs.iter().find(|attribute| {
        let path = attribute.path();
        path.is_ident("used")
            || path.is_ident("no_mangle")
            || path.is_ident("export_name")
            || path.is_ident("link_section")
    }) {
        return Err(syn::Error::new_spanned(
            attribute,
            "ELM import 槽的导出名和段属性由 #[elm::import] 独占管理",
        ));
    }
    let expected = match mode {
        "managed" => "ManagedImport",
        "direct-pinned" => "UnsafeDirectImport",
        _ => return Err(syn::Error::new(Span::call_site(), "未知 import mode")),
    };
    let Type::Path(path) = item.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &item.ty,
            format!("{mode} import 槽类型必须是 {expected}"),
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(syn::Error::new_spanned(&item.ty, "ELM import 槽类型无效"));
    };
    if path.qself.is_some()
        || segment.ident != expected
        || !matches!(segment.arguments, syn::PathArguments::None)
    {
        return Err(syn::Error::new_spanned(
            &item.ty,
            format!("{mode} import 槽类型必须是 {expected}"),
        ));
    }
    Ok(())
}

fn validate_point(value: &str) -> syn::Result<()> {
    validate_identifier(value, RELATION_POINT_LEN, "mixin point")
}

fn validate_function(function: &ItemFn, argument_count: usize) -> syn::Result<()> {
    if function.sig.constness.is_some()
        || function.sig.asyncness.is_some()
        || function.sig.unsafety.is_some()
        || function.sig.abi.is_some()
        || function.sig.variadic.is_some()
        || !function.sig.generics.params.is_empty()
    {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "ELM attribute 函数必须是非泛型安全 Rust 函数，不能手写 extern ABI",
        ));
    }
    if function.sig.inputs.len() != argument_count
        || function
            .sig
            .inputs
            .iter()
            .any(|argument| matches!(argument, FnArg::Receiver(_)))
    {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            format!("该 ELM attribute 要求恰好 {argument_count} 个普通参数"),
        ));
    }
    if matches!(function.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "ELM attribute 函数必须显式返回对应的 Result 类型",
        ));
    }
    Ok(())
}

fn mutable_reference_argument(function: &ItemFn) -> syn::Result<(Ident, Type)> {
    let Some(FnArg::Typed(argument)) = function.sig.inputs.first() else {
        return Err(syn::Error::new_spanned(
            &function.sig.inputs,
            "mixin 函数缺少帧参数",
        ));
    };
    let Pat::Ident(pattern) = argument.pat.as_ref() else {
        return Err(syn::Error::new_spanned(
            &argument.pat,
            "mixin 帧参数必须使用简单标识符",
        ));
    };
    let Type::Reference(reference) = argument.ty.as_ref() else {
        return Err(syn::Error::new_spanned(
            &argument.ty,
            "mixin 帧参数必须是可变借用",
        ));
    };
    if reference.mutability.is_none() {
        return Err(syn::Error::new_spanned(
            &argument.ty,
            "mixin 帧参数必须是可变借用",
        ));
    }
    Ok((pattern.ident.clone(), (*reference.elem).clone()))
}

fn require_empty_attr(attr: TokenStream2) -> syn::Result<()> {
    if attr.is_empty() {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(attr, "该 attribute 不接受参数"))
    }
}

#[derive(Clone)]
enum MetaValue {
    String(String),
    U32(u32),
    I32(i32),
    Bool(bool),
    Stages(u32),
}

struct MetaArgs {
    values: std::cell::RefCell<BTreeMap<String, MetaValue>>,
}

impl MetaArgs {
    fn parse(tokens: TokenStream2) -> syn::Result<Self> {
        let metas = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(tokens)?;
        let mut values = BTreeMap::new();
        for meta in metas {
            match meta {
                Meta::NameValue(value) => {
                    let Some(name) = value.path.get_ident().map(ToString::to_string) else {
                        return Err(syn::Error::new_spanned(value.path, "参数名必须是标识符"));
                    };
                    let parsed = parse_meta_value(value.value)?;
                    if values.insert(name.clone(), parsed).is_some() {
                        return Err(syn::Error::new_spanned(value.path, "重复 attribute 参数"));
                    }
                }
                Meta::List(list) if list.path.is_ident("stages") => {
                    let paths =
                        list.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)?;
                    let mut mask = 0u32;
                    for path in paths {
                        let Some(stage) = path.get_ident().map(ToString::to_string) else {
                            return Err(syn::Error::new_spanned(path, "stage 必须是标识符"));
                        };
                        let bit = 1 << (parse_stage(&stage)? - 1);
                        if mask & bit != 0 {
                            return Err(syn::Error::new_spanned(path, "stage 不能重复"));
                        }
                        mask |= bit;
                    }
                    if mask == 0
                        || values
                            .insert("stages".into(), MetaValue::Stages(mask))
                            .is_some()
                    {
                        return Err(syn::Error::new_spanned(list, "stages 不能为空或重复"));
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(other, "未知 ELM attribute 参数"));
                }
            }
        }
        Ok(Self {
            values: std::cell::RefCell::new(values),
        })
    }

    fn required_string(&self, name: &str) -> syn::Result<String> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::String(value)) if !value.is_empty() => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是非空字符串"),
            )),
            None => Err(syn::Error::new(
                Span::call_site(),
                format!("缺少必需参数 {name}"),
            )),
        }
    }

    fn string_or(&self, name: &str, default: &str) -> syn::Result<String> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::String(value)) if !value.is_empty() => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是非空字符串"),
            )),
            None => Ok(default.to_string()),
        }
    }

    fn required_u32(&self, name: &str) -> syn::Result<u32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::U32(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 u32 字面量"),
            )),
            None => Err(syn::Error::new(
                Span::call_site(),
                format!("缺少必需参数 {name}"),
            )),
        }
    }

    fn u32_or(&self, name: &str, default: u32) -> syn::Result<u32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::U32(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 u32 字面量"),
            )),
            None => Ok(default),
        }
    }

    fn i32_or(&self, name: &str, default: i32) -> syn::Result<i32> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::I32(value)) => Ok(value),
            Some(MetaValue::U32(value)) => {
                i32::try_from(value).map_err(|_| syn::Error::new(Span::call_site(), "i32 参数越界"))
            }
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是 i32 字面量"),
            )),
            None => Ok(default),
        }
    }

    fn bool_or(&self, name: &str, default: bool) -> syn::Result<bool> {
        match self.values.borrow_mut().remove(name) {
            Some(MetaValue::Bool(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(
                Span::call_site(),
                format!("{name} 必须是布尔字面量"),
            )),
            None => Ok(default),
        }
    }

    fn stages(&self) -> syn::Result<u32> {
        match self.values.borrow_mut().remove("stages") {
            Some(MetaValue::Stages(value)) => Ok(value),
            Some(_) => Err(syn::Error::new(Span::call_site(), "stages 格式无效")),
            None => Ok(0b1111),
        }
    }

    fn finish(&self) -> syn::Result<()> {
        if let Some(name) = self.values.borrow().keys().next() {
            Err(syn::Error::new(
                Span::call_site(),
                format!("未知或未使用的 attribute 参数 {name}"),
            ))
        } else {
            Ok(())
        }
    }
}

fn parse_meta_value(value: Expr) -> syn::Result<MetaValue> {
    match value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Ok(MetaValue::String(value.value())),
        Expr::Lit(ExprLit {
            lit: Lit::Int(value),
            ..
        }) => parse_integer_literal(value),
        Expr::Lit(ExprLit {
            lit: Lit::Bool(value),
            ..
        }) => Ok(MetaValue::Bool(value.value)),
        Expr::Unary(ExprUnary {
            op: UnOp::Neg(_),
            expr,
            ..
        }) => {
            let Expr::Lit(ExprLit {
                lit: Lit::Int(value),
                ..
            }) = *expr
            else {
                return Err(syn::Error::new_spanned(expr, "负数参数必须是整数常量"));
            };
            parse_negative_integer_literal(value)
        }
        other => Err(syn::Error::new_spanned(
            other,
            "ELM attribute 只接受字符串、整数和布尔字面量",
        )),
    }
}

fn parse_integer_literal(value: syn::LitInt) -> syn::Result<MetaValue> {
    let digits = value.base10_digits();
    if digits.starts_with('-') {
        parse_negative_integer_literal(value)
    } else {
        let parsed = digits
            .parse::<u32>()
            .map_err(|_| syn::Error::new_spanned(&value, "u32 参数越界"))?;
        Ok(MetaValue::U32(parsed))
    }
}

fn parse_negative_integer_literal(value: syn::LitInt) -> syn::Result<MetaValue> {
    let digits = value
        .base10_digits()
        .strip_prefix('-')
        .unwrap_or(value.base10_digits());
    let magnitude = digits
        .parse::<u64>()
        .map_err(|_| syn::Error::new_spanned(&value, "负数参数必须是十进制整数"))?;
    if magnitude > i32::MAX as u64 + 1 {
        return Err(syn::Error::new_spanned(value, "i32 参数越界"));
    }
    Ok(MetaValue::I32(-(magnitude as i64) as i32))
}

enum WireType {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    Bool,
    Bytes(usize),
}

impl WireType {
    fn parse(ty: &Type) -> syn::Result<Self> {
        match ty {
            Type::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                match path.path.segments[0].ident.to_string().as_str() {
                    "u8" => Ok(Self::U8),
                    "u16" => Ok(Self::U16),
                    "u32" => Ok(Self::U32),
                    "u64" => Ok(Self::U64),
                    "i8" => Ok(Self::I8),
                    "i16" => Ok(Self::I16),
                    "i32" => Ok(Self::I32),
                    "i64" => Ok(Self::I64),
                    "bool" => Ok(Self::Bool),
                    _ => Err(syn::Error::new_spanned(
                        ty,
                        "载荷字段只允许定宽整数、bool 和 [u8; N]",
                    )),
                }
            }
            Type::Array(array) => {
                let Type::Path(element) = array.elem.as_ref() else {
                    return Err(syn::Error::new_spanned(ty, "数组元素必须是 u8"));
                };
                if !element.path.is_ident("u8") {
                    return Err(syn::Error::new_spanned(ty, "数组元素必须是 u8"));
                }
                let Expr::Lit(ExprLit {
                    lit: Lit::Int(length),
                    ..
                }) = &array.len
                else {
                    return Err(syn::Error::new_spanned(
                        &array.len,
                        "数组长度必须是整数字面量",
                    ));
                };
                Ok(Self::Bytes(length.base10_parse()?))
            }
            _ => Err(syn::Error::new_spanned(
                ty,
                "载荷字段禁止引用、指针、usize、浮点、动态容器和泛型",
            )),
        }
    }

    const fn size(&self) -> usize {
        match self {
            Self::U8 | Self::I8 | Self::Bool => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 => 4,
            Self::U64 | Self::I64 => 8,
            Self::Bytes(length) => *length,
        }
    }

    fn encoder(&self, ident: &Ident) -> TokenStream2 {
        match self {
            Self::U8 => quote! {
                ::elm::developer::__private::write_bytes(output, &mut offset, &[self.#ident])?;
            },
            Self::I8 => quote! {
                ::elm::developer::__private::write_bytes(
                    output,
                    &mut offset,
                    &self.#ident.to_le_bytes(),
                )?;
            },
            Self::Bool => quote! {
                ::elm::developer::__private::write_bytes(
                    output,
                    &mut offset,
                    &[u8::from(self.#ident)],
                )?;
            },
            Self::U16 | Self::U32 | Self::U64 | Self::I16 | Self::I32 | Self::I64 => quote! {
                ::elm::developer::__private::write_bytes(
                    output,
                    &mut offset,
                    &self.#ident.to_le_bytes(),
                )?;
            },
            Self::Bytes(_) => quote! {
                ::elm::developer::__private::write_bytes(output, &mut offset, &self.#ident)?;
            },
        }
    }

    fn decoder(&self, ident: &Ident) -> TokenStream2 {
        match self {
            Self::U8 => quote! {
                #ident: ::elm::developer::__private::read_array::<1>(input, &mut offset)?[0]
            },
            Self::I8 => quote! {
                #ident: i8::from_le_bytes(
                    ::elm::developer::__private::read_array::<1>(input, &mut offset)?,
                )
            },
            Self::U16 => decode_integer(ident, quote!(u16), 2),
            Self::U32 => decode_integer(ident, quote!(u32), 4),
            Self::U64 => decode_integer(ident, quote!(u64), 8),
            Self::I16 => decode_integer(ident, quote!(i16), 2),
            Self::I32 => decode_integer(ident, quote!(i32), 4),
            Self::I64 => decode_integer(ident, quote!(i64), 8),
            Self::Bool => quote! {
                #ident: ::elm::developer::__private::read_bool(input, &mut offset)?
            },
            Self::Bytes(length) => quote! {
                #ident: ::elm::developer::__private::read_array::<#length>(input, &mut offset)?
            },
        }
    }
}

fn decode_integer(ident: &Ident, ty: TokenStream2, size: usize) -> TokenStream2 {
    quote! {
        #ident: #ty::from_le_bytes(
            ::elm::developer::__private::read_array::<#size>(input, &mut offset)?,
        )
    }
}

struct MetaField {
    tag: u16,
    kind: u16,
    bytes: Vec<u8>,
}

impl MetaField {
    fn utf8(tag: u16, value: &str) -> Self {
        Self {
            tag,
            kind: VALUE_UTF8,
            bytes: value.as_bytes().to_vec(),
        }
    }

    fn u32(tag: u16, value: u32) -> Self {
        Self {
            tag,
            kind: VALUE_U32,
            bytes: value.to_le_bytes().to_vec(),
        }
    }

    fn i32(tag: u16, value: i32) -> Self {
        Self {
            tag,
            kind: VALUE_I32,
            bytes: value.to_le_bytes().to_vec(),
        }
    }
}

fn metadata_record(kind: u16, mut fields: Vec<MetaField>) -> Vec<u8> {
    fields.sort_by_key(|field| field.tag);
    let mut payload = Vec::new();
    for field in &fields {
        payload.extend_from_slice(&field.tag.to_le_bytes());
        payload.extend_from_slice(&field.kind.to_le_bytes());
        payload.extend_from_slice(&(field.bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(&field.bytes);
        while payload.len() % 8 != 0 {
            payload.push(0);
        }
    }
    let record_size = META_HEADER_SIZE + payload.len();
    let mut output = Vec::with_capacity(record_size);
    output.extend_from_slice(META_MAGIC);
    output.extend_from_slice(&META_VERSION.to_le_bytes());
    output.extend_from_slice(&kind.to_le_bytes());
    output.extend_from_slice(&(META_HEADER_SIZE as u16).to_le_bytes());
    output.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    output.extend_from_slice(&(record_size as u32).to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&crc32(&payload).to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&payload);
    output
}

fn metadata_blob(records: Vec<Vec<u8>>) -> Vec<u8> {
    let total = records.iter().map(Vec::len).sum();
    let mut output = Vec::with_capacity(total);
    for record in records {
        output.extend_from_slice(&record);
    }
    output
}

fn metadata_item(anchor: &Ident, suffix: &str, bytes: Vec<u8>) -> TokenStream2 {
    let suffix = sanitize_ident(suffix);
    let align_ident = format_ident!("__ElmMetaAlign_{}_{}", anchor, suffix);
    let static_ident = format_ident!("__ELM_META_{}_{}", anchor, suffix);
    let length = bytes.len();
    let values = bytes.iter();
    quote! {
        #[doc(hidden)]
        #[repr(C, align(8))]
        struct #align_ident([u8; #length]);

        #[doc(hidden)]
        #[used]
        #[allow(non_upper_case_globals)]
        #[unsafe(link_section = ".elm.meta")]
        static #static_ident: #align_ident = #align_ident([#(#values),*]);
    }
}

fn sanitize_ident(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

use syn::parse::Parser as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_negative_i32_attribute_values() {
        let args = MetaArgs::parse(quote!(priority = -2147483648)).unwrap();
        assert_eq!(args.i32_or("priority", 0).unwrap(), i32::MIN);
        assert!(args.finish().is_ok());
    }

    #[test]
    fn rejects_duplicate_mixin_stages() {
        assert!(MetaArgs::parse(quote!(stages(ingress, ingress))).is_err());
    }

    #[test]
    fn validates_final_mixin_point_length() {
        assert!(validate_point("short.ingress").is_ok());
        assert!(validate_point("this-point-name-is-too-long.ingress").is_err());
    }

    #[test]
    fn validates_import_slot_type_against_mode() {
        let managed: ItemStatic = syn::parse_quote! {
            static REMOTE: ::elm::ManagedImport = ::elm::ManagedImport::new();
        };
        let direct: ItemStatic = syn::parse_quote! {
            static REMOTE: ::elm::UnsafeDirectImport = ::elm::UnsafeDirectImport::new();
        };
        assert!(validate_import_slot(&managed, "managed").is_ok());
        assert!(validate_import_slot(&direct, "direct-pinned").is_ok());
        assert!(validate_import_slot(&managed, "direct-pinned").is_err());
    }

    #[test]
    fn rejects_contract_with_empty_version_component() {
        assert!(validate_contract("test.contract@1.0").is_ok());
        assert!(validate_contract("test.contract@1..0").is_err());
        assert!(validate_contract("test.contract@.").is_err());
    }
}
