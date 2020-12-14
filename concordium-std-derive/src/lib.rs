// #![no_std]
extern crate proc_macro;
extern crate syn;
#[macro_use]
extern crate quote;

use proc_macro::TokenStream;
use quote::ToTokens;
use syn::{
    export::Span, parse::Parser, parse_macro_input, punctuated::*, spanned::Spanned, Ident, Meta,
    Token,
};

/// A helper to report meaningful compilation errors
/// - If applied to an Ok value they simply return the underlying value.
/// - If applied to `Err(e)` then `e` is turned into a compiler error.
fn unwrap_or_report(v: syn::Result<TokenStream>) -> TokenStream {
    match v {
        Ok(ts) => ts,
        Err(e) => e.to_compile_error().into(),
    }
}

fn attach_error<A>(mut v: syn::Result<A>, msg: &str) -> syn::Result<A> {
    if let Err(e) = v.as_mut() {
        let span = e.span();
        e.combine(syn::Error::new(span, msg));
    }
    v
}

/// Get the name item from a list, if available and a string literal.
/// If the named item does not have the expected (string) value, this will
/// return an Err. If the item does not exist the return value is Ok(None).
/// FIXME: Ensure there is only one.
fn get_attribute_value<'a, I: IntoIterator<Item = &'a Meta>>(
    iter: I,
    name: &str,
) -> syn::Result<Option<String>> {
    for attr in iter.into_iter() {
        match attr {
            Meta::NameValue(mnv) => {
                if mnv.path.is_ident(name) {
                    if let syn::Lit::Str(lit) = &mnv.lit {
                        return Ok(Some(lit.value()));
                    } else {
                        return Err(syn::Error::new(
                            mnv.span(),
                            format!("The `{}` attribute must be a string literal.", name),
                        ));
                    }
                }
            }
            Meta::Path(p) => {
                if p.is_ident(name) {
                    return Err(syn::Error::new(
                        attr.span(),
                        format!("The `{}` attribute must have a string literal value.", name),
                    ));
                }
            }
            Meta::List(p) => {
                if p.path.is_ident(name) {
                    return Err(syn::Error::new(
                        attr.span(),
                        format!("The `{}` attribute must have a string literal value.", name),
                    ));
                }
            }
        }
    }
    Ok(None)
}

// Return whether a attribute item is present.
fn contains_attribute<'a, I: IntoIterator<Item = &'a Meta>>(iter: I, name: &str) -> bool {
    iter.into_iter().any(|attr| attr.path().is_ident(name))
}

/// Derive the appropriate export for an annotated init function.
///
/// This macro requires the following items to be present
/// - `contract="<name>"` where *\<name\>* is the name of the smart contract and
///   the generated function is exported as this name prefixed with *init_*. The
///   name should be unique in the module, as a contract can only have one
///   init-function.
///
/// The annotated function must be of a specific type, which depends on the
/// enabled attributes. *Without* any of the optional attributes the function
/// must have a signature of
///
/// ```ignore
/// #[init(contract = "my_contract")]
/// fn some_init(ctx: &impl HasInitContext) -> InitResult<MyState> {...}
/// ```
///
/// Where the trait `HasInitContext` and the type `InitResult` are exposed from
/// `concordium-std` and `MyState` is the user-defined type for the contract
/// state.
///
/// # Optional attributes
///
/// ## `payable`: Make function accept an amount of GTU
/// Without setting the `payable` attribute, the generated function will reject
/// any non-zero amount of GTU supplied with the transaction. This means we are
/// required to explicitly mark our functions as `payable`, if they are to
/// accept GTU.
///
/// Setting the `payable` attribute changes the required signature to include an
/// extra argument of type `Amount`, allowing the function to access the amount
/// of GTU supplied with the transaction.
///
/// ### Example
/// ```ignore
/// #[init(contract = "my_contract", payable)]
/// fn some_init(ctx: &impl HasInitContext, amount: Amount) -> InitResult<MyState> {...}
/// ```
///
/// ## `enable_logger`: Function can access event logging
/// Setting the `enable_logger` attribute changes the required signature to
/// include an extra argument `&mut impl HasLogger`, allowing the function to
/// log events.
///
///
/// ### Example
/// ```ignore
/// #[init(contract = "my_contract", enable_logger)]
/// fn some_init(ctx: &impl HasInitContext, logger: &mut impl HasLogger) -> InitResult<MyState> {...}
/// ```
///
/// ## `low_level`: Manually deal with writing state bytes
/// Setting the `low_level` attribute disables the generated code for
/// serializing the contract state.
///
/// If `low_level` is set, the signature must contain an extra argument of type
/// `&mut ContractState` found in `concordium-std`, which gives access to
/// manipulating the contract state bytes directly. This means there is no need
/// to return the contract state and the return type becomes `InitResult<()>`.
///
/// ### Example
/// ```ignore
/// #[init(contract = "my_contract", low_level)]
/// fn some_init(ctx: &impl HasInitContext, state: &mut ContractState) -> InitResult<()> {...}
/// ```
///
/// ## `parameter="<Param>"`: Generate schema for parameter
/// To make schema generation to include the parameter for this function, add
/// the attribute `parameter` and set it equal to a string literal containing
/// the name of the type used for the parameter. The parameter type must
/// implement the SchemaType trait, which for most cases can be derived
/// automatically.
///
/// ### Example
/// ```ignore
/// #[derive(SchemaType)]
/// struct MyParam { ... }
///
/// #[init(contract = "my_contract", parameter = "MyParam")]
/// ```
#[proc_macro_attribute]
pub fn init(attr: TokenStream, item: TokenStream) -> TokenStream {
    unwrap_or_report(init_worker(attr, item))
}

fn init_worker(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let attrs = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;

    let contract_name = get_attribute_value(attrs.iter(), "contract")?.ok_or_else(|| {
        syn::Error::new(
            attrs.span(),
            "A name for the contract must be provided, using the contract attribute.For example, \
             #[init(contract = \"my-contract\")]",
        )
    })?;

    let ast: syn::ItemFn =
        attach_error(syn::parse(item), "#[init] can only be applied to functions.")?;

    let fn_name = &ast.sig.ident;
    let rust_export_fn_name = format_ident!("export_{}", fn_name);
    let wasm_export_fn_name = format!("init_{}", contract_name);
    let amount_ident = format_ident!("amount");

    // Accumulate a list of required arguments, if the function contains a
    // different number of arguments, than elements in this vector, then the
    // strings are displayed as the expected arguments.
    let mut required_args = vec!["ctx: &impl HasInitContext"];

    let (setup_fn_optional_args, fn_optional_args) =
        contract_function_optional_args_tokens(&attrs, &amount_ident, &mut required_args);

    let mut out = if contains_attribute(attrs.iter(), "low_level") {
        required_args.push("state: &mut ContractState");
        quote! {
            #[export_name = #wasm_export_fn_name]
            pub extern "C" fn #rust_export_fn_name(#amount_ident: Amount) -> i32 {
                use concordium_std::{trap, ExternContext, InitContextExtern, ContractState};
                #setup_fn_optional_args
                let ctx = ExternContext::<InitContextExtern>::open(());
                let mut state = ContractState::open(());
                match #fn_name(&ctx, #(#fn_optional_args, )* &mut state) {
                    Ok(()) => 0,
                    Err(_) => -1,
                }
            }
        }
    } else {
        quote! {
            #[export_name = #wasm_export_fn_name]
            pub extern "C" fn #rust_export_fn_name(amount: Amount) -> i32 {
                use concordium_std::{trap, ExternContext, InitContextExtern, ContractState};
                #setup_fn_optional_args
                let ctx = ExternContext::<InitContextExtern>::open(());
                match #fn_name(&ctx, #(#fn_optional_args),*) {
                    Ok(state) => {
                        let mut state_bytes = ContractState::open(());
                        if state.serial(&mut state_bytes).is_err() {
                            trap() // Could not initialize contract.
                        };
                        0
                    }
                    Err(_) => -1
                }
            }
        }
    };

    let arg_count = ast.sig.inputs.len();
    if arg_count != required_args.len() {
        return Err(syn::Error::new(
            ast.sig.inputs.span(),
            format!(
                "Incorrect number of function arguments, the expected arguments are ({}) ",
                required_args.join(", ")
            ),
        ));
    }

    // Embed schema if 'parameter' attribute is set
    let parameter_option = get_attribute_value(attrs.iter(), "parameter")?;
    out.extend(contract_function_schema_tokens(
        parameter_option,
        rust_export_fn_name,
        wasm_export_fn_name,
    ));

    ast.to_tokens(&mut out);

    Ok(out.into())
}

/// Derive the appropriate export for an annotated receive function.
///
/// This macro requires the following items to be present
/// - `contract = "<contract-name>"` where *\<contract-name\>* is the name of
///   the smart contract.
/// - `name = "<receive-name>"` where *\<receive-name\>* is the name of the
///   receive function. The generated function is exported as
///   `<contract-name>.<receive-name>`. Contract name and receive name is
///   required to be unique in the module.
///
/// The annotated function must be of a specific type, which depends on the
/// enabled attributes. *Without* any of the optional attributes the function
/// must have a signature of
///
/// ```ignore
/// #[receive(contract = "my_contract", name = "some_receive")]
/// fn contract_receive<A: HasActions>(ctx: &impl HasReceiveContext, state: &mut MyState) -> ReceiveResult<A> {...}
/// ```
///
/// Where the `HasAction`, `HasReceiveContext` traits and the type
/// `ReceiveResult` are exposed from `concordium-std` and `MyState` is the
/// user-defined type for the contract state.
///
/// # Optional attributes
///
/// ## `payable`: Make function accept an amount of GTU
/// Without setting the `payable` attribute, the function will reject any
/// non-zero amount of GTU, supplied with the transaction. This means we are
/// required to explicitly mark our functions as `payable`, if they are to
/// accept GTU.
///
/// Setting the `payable` attribute changes the required signature to include an
/// extra argument of type `Amount`, allowing the function to access the amount
/// of GTU supplied with the transaction.
///
/// ### Example
/// ```ignore
/// #[receive(contract = "my_contract", name = "some_receive", payable)]
/// fn contract_receive<A: HasActions>(ctx: &impl HasReceiveContext, amount: Amount, state: &mut MyState) -> ReceiveResult<A> {...}
/// ```
///
/// ## `enable_logger`: Function can access event logging
/// Setting the `enable_logger` attribute changes the required signature to
/// include an extra argument `&mut impl HasLogger`, allowing the function to
/// log events.
///
///
/// ### Example
/// ```ignore
/// #[receive(contract = "my_contract", name = "some_receive", enable_logger)]
/// fn contract_receive<A: HasActions>(ctx: &impl HasReceiveContext, logger: &mut impl HasLogger, state: &mut MyState) -> ReceiveResult<A> {...}
/// ```
///
/// ## `low_level`: Manually deal with writing state bytes
/// Setting the `low_level` attribute disables the generated code for
/// serializing the contract state.
///
/// If `low_level` is set, instead of the user-defined state type in the
/// signature, the state argument becomes the type `&mut ContractState` found in
/// `concordium-std`, which gives access to manipulating the contract state
/// bytes directly.
///
/// ### Example
/// ```ignore
/// #[receive(contract = "my_contract", name = "some_receive", low_level)]
/// fn contract_receive<A: HasActions>(ctx: &impl HasReceiveContext, state: &mut ContractState) -> ReceiveResult<A> {...}
/// ```
///
/// ## `parameter="<Param>"`: Generate schema for parameter
/// To make schema generation include the parameter for this function, add
/// the attribute `parameter` and set it equal to a string literal containing
/// the name of the type used for the parameter. The parameter type must
/// implement the SchemaType trait, which for most cases can be derived
/// automatically.
///
/// ### Example
/// ```ignore
/// #[derive(SchemaType)]
/// struct MyParam { ... }
///
/// #[receive(contract = "my_contract", name = "some_receive", parameter = "MyParam")]
/// fn contract_receive<A: HasActions>(ctx: &impl HasReceiveContext, state: &mut MyState) -> ReceiveResult<A> {...}
/// ```
#[proc_macro_attribute]
pub fn receive(attr: TokenStream, item: TokenStream) -> TokenStream {
    unwrap_or_report(receive_worker(attr, item))
}

fn receive_worker(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let attrs = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;

    let contract_name = get_attribute_value(attrs.iter(), "contract")?.ok_or_else(|| {
        syn::Error::new(
            attrs.span(),
            "The name of the associated contract must be provided, using the 'contract' \
             attribute.\n\nFor example, #[receive(contract = \"my-contract\")]",
        )
    })?;

    let name = get_attribute_value(attrs.iter(), "name")?.ok_or_else(|| {
        syn::Error::new(
            attrs.span(),
            "A name for the receive function must be provided, using the 'name' attribute.\n\nFor \
             example, #[receive(name = \"func-name\", ...)]",
        )
    })?;

    let ast: syn::ItemFn =
        attach_error(syn::parse(item), "#[receive] can only be applied to functions.")?;

    let fn_name = &ast.sig.ident;
    let rust_export_fn_name = format_ident!("export_{}", fn_name);
    let wasm_export_fn_name = format!("{}.{}", contract_name, name);
    let amount_ident = format_ident!("amount");

    // Accumulate a list of required arguments, if the function contains a
    // different number of arguments, than elements in this vector, then the
    // strings are displayed as the expected arguments.
    let mut required_args = vec!["ctx: &impl HasReceiveContext"];

    let (setup_fn_optional_args, fn_optional_args) =
        contract_function_optional_args_tokens(&attrs, &amount_ident, &mut required_args);

    let mut out = if contains_attribute(&attrs, "low_level") {
        required_args.push("state: &mut ContractState");
        quote! {
            #[export_name = #wasm_export_fn_name]
            pub extern "C" fn #rust_export_fn_name(#amount_ident: Amount) -> i32 {
                use concordium_std::{SeekFrom, ContractState, Logger, ReceiveContextExtern, ExternContext};
                #setup_fn_optional_args
                let ctx = ExternContext::<ReceiveContextExtern>::open(());
                let mut state = ContractState::open(());
                let res: Result<Action, _> = #fn_name(&ctx, #(#fn_optional_args, )* &mut state);
                match res {
                    Ok(act) => {
                        act.tag() as i32
                    }
                    Err(_) => -1,
                }
            }
        }
    } else {
        required_args.push("state: &mut MyState");

        quote! {
            #[export_name = #wasm_export_fn_name]
            pub extern "C" fn #rust_export_fn_name(#amount_ident: Amount) -> i32 {
                use concordium_std::{SeekFrom, ContractState, Logger, trap};
                #setup_fn_optional_args
                let ctx = ExternContext::<ReceiveContextExtern>::open(());
                let mut state_bytes = ContractState::open(());
                if let Ok(mut state) = (&mut state_bytes).get() {
                    let res: Result<Action, _> = #fn_name(&ctx, #(#fn_optional_args, )* &mut state);
                    match res {
                        Ok(act) => {
                            let res = state_bytes
                                .seek(SeekFrom::Start(0))
                                .and_then(|_| state.serial(&mut state_bytes));
                            if res.is_err() {
                                trap() // could not serialize state.
                            } else {
                                act.tag() as i32
                            }
                        }
                        Err(_) => -1,
                    }
                } else {
                    trap() // Could not fully read state.
                }
            }
        }
    };

    let arg_count = ast.sig.inputs.len();
    if arg_count != required_args.len() {
        return Err(syn::Error::new(
            ast.sig.inputs.span(),
            format!(
                "Incorrect number of function arguments, the expected arguments are ({}) ",
                required_args.join(", ")
            ),
        ));
    }

    // Embed schema if 'parameter' attribute is set
    let parameter_option = get_attribute_value(attrs.iter(), "parameter")?;
    out.extend(contract_function_schema_tokens(
        parameter_option,
        rust_export_fn_name,
        wasm_export_fn_name,
    ));
    // add the original function to the output as well.
    ast.to_tokens(&mut out);
    Ok(out.into())
}

/// Generate tokens for some of the optional arguments, based on the attributes.
/// Returns a pair, where the first entry is tokens for setting up the arguments
/// and the second entry is a Vec of the argument names as tokens.
///
/// It also mutates a vector of required arguments with the expected type
/// signature of each.
fn contract_function_optional_args_tokens<'a, I: Copy + IntoIterator<Item = &'a Meta>>(
    attrs: I,
    amount_ident: &syn::Ident,
    required_args: &mut Vec<&str>,
) -> (proc_macro2::TokenStream, Vec<proc_macro2::TokenStream>) {
    let mut setup_fn_args = proc_macro2::TokenStream::new();
    let mut fn_args = vec![];
    if contains_attribute(attrs, "payable") {
        required_args.push("amount: Amount");
        fn_args.push(quote!(#amount_ident));
    } else {
        setup_fn_args.extend(quote! {
            if #amount_ident.micro_gtu != 0 {
                return -1;
            }
        });
    };

    if contains_attribute(attrs, "enable_logger") {
        required_args.push("logger: &mut impl HasLogger");
        let logger_ident = format_ident!("logger");
        setup_fn_args.extend(quote!(let mut #logger_ident = concordium_std::Logger::init();));
        fn_args.push(quote!(&mut #logger_ident));
    }
    (setup_fn_args, fn_args)
}

#[cfg(feature = "build-schema")]
fn contract_function_schema_tokens(
    parameter_option: Option<String>,
    rust_name: syn::Ident,
    wasm_name: String,
) -> proc_macro2::TokenStream {
    match parameter_option {
        Some(parameter_ty) => {
            let parameter_ident = syn::Ident::new(&parameter_ty, Span::call_site());
            let schema_name = format!("concordium_schema_function_{}", wasm_name);
            let schema_ident = format_ident!("concordium_schema_function_{}", rust_name);
            quote! {
                #[export_name = #schema_name]
                pub extern "C" fn #schema_ident() -> *mut u8 {
                    let schema = <#parameter_ident as schema::SchemaType>::get_type();
                    let schema_bytes = concordium_std::to_bytes(&schema);
                    concordium_std::put_in_memory(&schema_bytes)
                }
            }
        }
        None => proc_macro2::TokenStream::new(),
    }
}

#[cfg(not(feature = "build-schema"))]
fn contract_function_schema_tokens(
    _parameter_option: Option<String>,
    _rust_name: syn::Ident,
    _wasm_name: String,
) -> proc_macro2::TokenStream {
    proc_macro2::TokenStream::new()
}

/// Derive the Deserial trait. See the documentation of `derive(Serial)` for
/// details and limitations.
///
/// In addition to the attributes supported by `derive(Serial)`, this derivation
/// macro supports the `ensure_ordered` attribute. If applied to a field the
/// of type `BTreeMap` or `BTreeSet` deserialization will additionally ensure
/// that there keys are in strictly increasing order. By default deserialization
/// only ensures uniqueness.
///
/// # Example
/// ``` ignore
/// #[derive(Deserial)]
/// struct Foo {
///     #[concordium(set_size_length = 1, ensure_ordered)]
///     bar: BTreeSet<u8>,
/// }
/// ```
#[proc_macro_derive(Deserial, attributes(concordium))]
pub fn deserial_derive(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input);
    unwrap_or_report(impl_deserial(&ast))
}

/// The prefix used in field attributes: `#[concordium(attr = "something")]`
const CONCORDIUM_FIELD_ATTRIBUTE: &str = "concordium";

/// A list of valid concordium field attributes
const VALID_CONCORDIUM_FIELD_ATTRIBUTES: [&str; 5] =
    ["size_length", "set_size_length", "map_size_length", "string_size_length", "ensure_ordered"];

fn get_concordium_field_attributes(attributes: &[syn::Attribute]) -> syn::Result<Vec<syn::Meta>> {
    attributes
        .iter()
        // Keep only concordium attributes
        .flat_map(|attr| match attr.parse_meta() {
            Ok(syn::Meta::List(list)) if list.path.is_ident(CONCORDIUM_FIELD_ATTRIBUTE) => {
                list.nested
            }
            _ => syn::punctuated::Punctuated::new(),
        })
        // Ensure only valid attributes and unwrap NestedMeta
        .map(|nested| match nested {
            syn::NestedMeta::Meta(meta) => {
                let path = meta.path();
                if VALID_CONCORDIUM_FIELD_ATTRIBUTES.iter().any(|&attr| path.is_ident(attr)) {
                    Ok(meta)
                } else {
                    Err(syn::Error::new(meta.span(),
                        format!("The attribute '{}' is not supported as a concordium field attribute.",
                        path.to_token_stream())
                    ))
                }
            }
            lit => Err(syn::Error::new(lit.span(), "Literals are not supported in a concordium field attribute.")),
        })
        .collect()
}

fn find_field_attribute_value(
    attributes: &[syn::Attribute],
    target_attr: &str,
) -> syn::Result<Option<syn::Lit>> {
    let target_attr = format_ident!("{}", target_attr);
    let attr_values: Vec<_> = get_concordium_field_attributes(attributes)?
        .into_iter()
        .filter_map(|nested_meta| match nested_meta {
            syn::Meta::NameValue(value) if value.path.is_ident(&target_attr) => Some(value.lit),
            _ => None,
        })
        .collect();
    if attr_values.is_empty() {
        return Ok(None);
    }
    if attr_values.len() > 1 {
        let mut init_error = syn::Error::new(
            attr_values[1].span(),
            format!("Attribute '{}' should only be specified once.", target_attr),
        );
        for other in attr_values.iter().skip(2) {
            init_error.combine(syn::Error::new(
                other.span(),
                format!("Attribute '{}' should only be specified once.", target_attr),
            ))
        }
        Err(init_error)
    } else {
        Ok(Some(attr_values[0].clone()))
    }
}

fn find_length_attribute(
    attributes: &[syn::Attribute],
    target_attr: &str,
) -> syn::Result<Option<u32>> {
    let value = match find_field_attribute_value(attributes, target_attr)? {
        Some(v) => v,
        None => return Ok(None),
    };

    let value = match value {
        syn::Lit::Int(int) => int,
        _ => {
            return Err(syn::Error::new(value.span(), "Length attribute value must be an integer."))
        }
    };
    let value = match value.base10_parse() {
        Ok(v) => v,
        _ => {
            return Err(syn::Error::new(
                value.span(),
                "Length attribute value must be a base 10 integer.",
            ))
        }
    };
    match value {
        1 | 2 | 4 | 8 => Ok(Some(value)),
        _ => Err(syn::Error::new(value.span(), "Length info must be either 1, 2, 4, or 8.")),
    }
}

fn impl_deserial_field(
    f: &syn::Field,
    ident: &syn::Ident,
    source: &syn::Ident,
) -> syn::Result<proc_macro2::TokenStream> {
    let concordium_attributes = get_concordium_field_attributes(&f.attrs)?;
    if let Some(l) = find_length_attribute(&f.attrs, "size_length")? {
        let size = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let #ident = {
                let len = #size::deserial(#source)?;
                deserial_vector_no_length(#source, len as usize)?
            };
        })
    } else if let Some(l) = find_length_attribute(&f.attrs, "map_size_length")? {
        let size = format_ident!("u{}", 8 * l);
        if contains_attribute(&concordium_attributes, "ensure_ordered") {
            Ok(quote! {
                let #ident = {
                    let len = #size::deserial(#source)?;
                    deserial_map_no_length(#source, len as usize)?
                };
            })
        } else {
            Ok(quote! {
                let #ident = {
                    let len = #size::deserial(#source)?;
                    deserial_map_no_length_no_order_check(#source, len as usize)?
                };
            })
        }
    } else if let Some(l) = find_length_attribute(&f.attrs, "set_size_length")? {
        let size = format_ident!("u{}", 8 * l);
        if contains_attribute(&concordium_attributes, "ensure_ordered") {
            Ok(quote! {
                let #ident = {
                    let len = #size::deserial(#source)?;
                    deserial_set_no_length(#source, len as usize)?
                };
            })
        } else {
            Ok(quote! {
                let #ident = {
                    let len = #size::deserial(#source)?;
                    deserial_set_no_length_no_order_check(#source, len as usize)?
                };
            })
        }
    } else if let Some(l) = find_length_attribute(&f.attrs, "string_size_length")? {
        let size = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let #ident = {
                let len = #size::deserial(#source)?;
                deserial_string(#source, len as usize)?
            };
        })
    } else {
        let ty = &f.ty;
        Ok(quote! {
            let #ident = <#ty as Deserial>::deserial(#source)?;
        })
    }
}

fn impl_deserial(ast: &syn::DeriveInput) -> syn::Result<TokenStream> {
    let data_name = &ast.ident;

    let span = ast.span();

    let read_ident = format_ident!("__R", span = span);

    let (impl_generics, ty_generics, where_clauses) = ast.generics.split_for_impl();

    let source_ident = Ident::new("source", Span::call_site());

    let body_tokens = match ast.data {
        syn::Data::Struct(ref data) => {
            let mut names = proc_macro2::TokenStream::new();
            let mut field_tokens = proc_macro2::TokenStream::new();
            let return_tokens = match data.fields {
                syn::Fields::Named(_) => {
                    for field in data.fields.iter() {
                        let field_ident = field.ident.clone().unwrap(); // safe since named fields.
                        field_tokens.extend(impl_deserial_field(
                            field,
                            &field_ident,
                            &source_ident,
                        ));
                        names.extend(quote!(#field_ident,))
                    }
                    quote!(Ok(#data_name{#names}))
                }
                syn::Fields::Unnamed(_) => {
                    for (i, f) in data.fields.iter().enumerate() {
                        let field_ident = format_ident!("x_{}", i);
                        field_tokens.extend(impl_deserial_field(f, &field_ident, &source_ident));
                        names.extend(quote!(#field_ident,))
                    }
                    quote!(Ok(#data_name(#names)))
                }
                _ => quote!(Ok(#data_name{})),
            };
            quote! {
                #field_tokens
                #return_tokens
            }
        }
        syn::Data::Enum(ref data) => {
            let mut matches_tokens = proc_macro2::TokenStream::new();
            let source = Ident::new("source", Span::call_site());
            let size = if data.variants.len() <= 256 {
                format_ident!("u8")
            } else if data.variants.len() <= 256 * 256 {
                format_ident!("u16")
            } else {
                return Err(syn::Error::new(
                    ast.span(),
                    "[derive(Deserial)]: Too many variants. Maximum 65536 are supported.",
                ));
            };
            for (i, variant) in data.variants.iter().enumerate() {
                let (field_names, pattern) = match variant.fields {
                    syn::Fields::Named(_) => {
                        let field_names: Vec<_> = variant
                            .fields
                            .iter()
                            .map(|field| field.ident.clone().unwrap())
                            .collect();
                        (field_names.clone(), quote! { {#(#field_names),*} })
                    }
                    syn::Fields::Unnamed(_) => {
                        let field_names: Vec<_> = variant
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, _)| format_ident!("x_{}", i))
                            .collect();
                        (field_names.clone(), quote! { ( #(#field_names),* ) })
                    }
                    syn::Fields::Unit => (Vec::new(), proc_macro2::TokenStream::new()),
                };

                let field_tokens: proc_macro2::TokenStream = field_names
                    .iter()
                    .zip(variant.fields.iter())
                    .map(|(name, field)| impl_deserial_field(field, name, &source))
                    .collect::<syn::Result<proc_macro2::TokenStream>>()?;
                let idx_lit = syn::LitInt::new(i.to_string().as_str(), Span::call_site());
                let variant_ident = &variant.ident;
                matches_tokens.extend(quote! {
                    #idx_lit => {
                        #field_tokens
                        Ok(#data_name::#variant_ident#pattern)
                    },
                })
            }
            quote! {
                let idx = #size::deserial(#source)?;
                match idx {
                    #matches_tokens
                    _ => Err(Default::default())
                }
            }
        }
        _ => unimplemented!("#[derive(Deserial)] is not implemented for union."),
    };
    let gen = quote! {
        #[automatically_derived]
        impl #impl_generics Deserial for #data_name #ty_generics #where_clauses {
            fn deserial<#read_ident: Read>(#source_ident: &mut #read_ident) -> ParseResult<Self> {
                #body_tokens
            }
        }
    };
    Ok(gen.into())
}

/// Derive the Serial trait for the type.
///
/// If the type is a struct all fields must implement the Serial trait. If the
/// type is an enum then all fields of each of the enums must implement the
/// Serial trait.
///
///
/// Collections (Vec, BTreeMap, BTreeSet) are by default serialized by
/// prepending the number of elements as 4 bytes little-endian. If this is too
/// much or too little, fields of the above types can be annotated with
/// `size_length` for Vec, `map_size_length` for `BTreeMap` and
/// `set_size_length` for `BTreeSet`.
///
/// The value of this field is the number of bytes that will be used for
/// encoding the number of elements. Supported values are 1, 2, 4, 8.
///
/// For BTreeMap and BTreeSet the serialize method will serialize values in
/// increasing order of keys.
///
/// Fields of structs are serialized in the order they appear in the code.
///
/// Enums can have no more than 65536 variants. They are serialized by using a
/// tag to indicate the variant, enumerating them in the order they are written
/// in source code. If the number of variants is less than or equal 256 then a
/// single byte is used to encode it. Otherwise two bytes are used for the tag,
/// encoded in little endian.
///
/// # Example
/// ```ignore
/// #[derive(Serial)]
/// struct Foo {
///     #[concordium(set_size_length = 1)]
///     bar: BTreeSet<u8>,
/// }
/// ```
#[proc_macro_derive(Serial, attributes(concordium))]
pub fn serial_derive(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input);
    unwrap_or_report(impl_serial(&ast))
}

fn impl_serial_field(
    field: &syn::Field,
    ident: &proc_macro2::TokenStream,
    out: &syn::Ident,
) -> syn::Result<proc_macro2::TokenStream> {
    if let Some(l) = find_length_attribute(&field.attrs, "size_length")? {
        let id = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let len: #id = #ident.len() as #id;
            len.serial(#out)?;
            serial_vector_no_length(&#ident, #out)?;
        })
    } else if let Some(l) = find_length_attribute(&field.attrs, "map_size_length")? {
        let id = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let len: #id = #ident.len() as #id;
            len.serial(#out)?;
            serial_map_no_length(&#ident, #out)?;
        })
    } else if let Some(l) = find_length_attribute(&field.attrs, "set_size_length")? {
        let id = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let len: #id = #ident.len() as #id;
            len.serial(#out)?;
            serial_set_no_length(&#ident, #out)?;
        })
    } else if let Some(l) = find_length_attribute(&field.attrs, "string_size_length")? {
        let id = format_ident!("u{}", 8 * l);
        Ok(quote! {
            let len: #id = #ident.len() as #id;
            len.serial(#out)?;
            serial_string(#ident.as_str(), #out)?;
        })
    } else {
        Ok(quote! {
            #ident.serial(#out)?;
        })
    }
}

fn impl_serial(ast: &syn::DeriveInput) -> syn::Result<TokenStream> {
    let data_name = &ast.ident;

    let span = ast.span();

    let write_ident = format_ident!("W", span = span);

    let (impl_generics, ty_generics, where_clauses) = ast.generics.split_for_impl();

    let out_ident = format_ident!("out");

    let body = match ast.data {
        syn::Data::Struct(ref data) => {
            let fields_tokens = match data.fields {
                syn::Fields::Named(_) => {
                    data.fields
                        .iter()
                        .map(|field| {
                            let field_ident = field.ident.clone().unwrap(); // safe since named fields.
                            let field_ident = quote!(self.#field_ident);
                            impl_serial_field(field, &field_ident, &out_ident)
                        })
                        .collect::<syn::Result<_>>()?
                }
                syn::Fields::Unnamed(_) => data
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, field)| {
                        let i = syn::LitInt::new(i.to_string().as_str(), Span::call_site());
                        let field_ident = quote!(self.#i);
                        impl_serial_field(field, &field_ident, &out_ident)
                    })
                    .collect::<syn::Result<_>>()?,
                syn::Fields::Unit => proc_macro2::TokenStream::new(),
            };
            quote! {
                #fields_tokens
                Ok(())
            }
        }
        syn::Data::Enum(ref data) => {
            let mut matches_tokens = proc_macro2::TokenStream::new();

            let size = if data.variants.len() <= 256 {
                format_ident!("u8")
            } else if data.variants.len() <= 256 * 256 {
                format_ident!("u16")
            } else {
                unimplemented!(
                    "[derive(Serial)]: Enums with more than 65536 variants are not supported."
                );
            };

            for (i, variant) in data.variants.iter().enumerate() {
                let (field_names, pattern) = match variant.fields {
                    syn::Fields::Named(_) => {
                        let field_names: Vec<_> = variant
                            .fields
                            .iter()
                            .map(|field| field.ident.clone().unwrap())
                            .collect();
                        (field_names.clone(), quote! { {#(#field_names),*} })
                    }
                    syn::Fields::Unnamed(_) => {
                        let field_names: Vec<_> = variant
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, _)| format_ident!("x_{}", i))
                            .collect();
                        (field_names.clone(), quote! { (#(#field_names),*) })
                    }
                    syn::Fields::Unit => (Vec::new(), proc_macro2::TokenStream::new()),
                };
                let field_tokens: proc_macro2::TokenStream = field_names
                    .iter()
                    .zip(variant.fields.iter())
                    .map(|(name, field)| impl_serial_field(field, &quote!(#name), &out_ident))
                    .collect::<syn::Result<_>>()?;

                let idx_lit =
                    syn::LitInt::new(format!("{}{}", i, size).as_str(), Span::call_site());
                let variant_ident = &variant.ident;

                matches_tokens.extend(quote! {
                    #data_name::#variant_ident#pattern => {
                        #idx_lit.serial(#out_ident)?;
                        #field_tokens
                    },
                })
            }
            quote! {
                match self {
                    #matches_tokens
                }
                Ok(())
            }
        }
        _ => unimplemented!("#[derive(Serial)] is not implemented for union."),
    };

    let gen = quote! {
        #[automatically_derived]
        impl #impl_generics Serial for #data_name #ty_generics #where_clauses {
            fn serial<#write_ident: Write>(&self, #out_ident: &mut #write_ident) -> Result<(), #write_ident::Err> {
                #body
            }
        }
    };
    Ok(gen.into())
}

/// A helper macro to derive both the Serial and Deserial traits.
/// `[derive(Serialize)]` is equivalent to `[derive(Serial,Deserial)]`, see
/// documentation of the latter two for details and options.
#[proc_macro_derive(Serialize, attributes(concordium))]
pub fn serialize_derive(input: TokenStream) -> TokenStream {
    unwrap_or_report(serialize_derive_worker(input))
}

fn serialize_derive_worker(input: TokenStream) -> syn::Result<TokenStream> {
    let ast = syn::parse(input)?;
    let mut tokens = impl_deserial(&ast)?;
    tokens.extend(impl_serial(&ast)?);
    Ok(tokens)
}

/// Marks a type as the contract state. So far only used for generating the
/// schema of the contract state.
///
///
/// # Example
/// ```ignore
/// #[contract_state(contract = "my_contract")]
/// #[derive(SchemaType)]
/// struct MyContractState {
///      ...
/// }
/// ```
#[cfg(feature = "build-schema")]
#[proc_macro_attribute]
pub fn contract_state(attr: TokenStream, item: TokenStream) -> TokenStream {
    unwrap_or_report(contract_state_worker(attr, item))
}

#[cfg(feature = "build-schema")]
fn contract_state_worker(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut out = proc_macro2::TokenStream::new();

    let data_ident = if let Ok(ast) = syn::parse::<syn::ItemStruct>(item.clone()) {
        ast.to_tokens(&mut out);
        ast.ident
    } else if let Ok(ast) = syn::parse::<syn::ItemEnum>(item.clone()) {
        ast.to_tokens(&mut out);
        ast.ident
    } else if let Ok(ast) = syn::parse::<syn::ItemType>(item.clone()) {
        ast.to_tokens(&mut out);
        ast.ident
    } else {
        return Err(syn::Error::new_spanned(
            proc_macro2::TokenStream::from(item),
            "#[contract_state] only supports structs, enums and type aliases.",
        ));
    };

    let attrs = Punctuated::<Meta, Token![,]>::parse_terminated.parse(attr)?;

    let contract_name = get_attribute_value(attrs.iter(), "contract")?.ok_or_else(|| {
        syn::Error::new(
            attrs.span(),
            "A name of the contract must be provided, using the 'contract' attribute.\n\nFor \
             example #[contract_state(contract = \"my-contract\")].",
        )
    })?;

    let wasm_schema_name = format!("concordium_schema_state_{}", contract_name);
    let rust_schema_name = format_ident!("concordium_schema_state_{}", data_ident);

    let generate_schema_tokens = quote! {
        #[allow(non_snake_case)]
        #[export_name = #wasm_schema_name]
        pub extern "C" fn #rust_schema_name() -> *mut u8 {
            let schema = <#data_ident as concordium_std::schema::SchemaType>::get_type();
            let schema_bytes = concordium_std::to_bytes(&schema);
            concordium_std::put_in_memory(&schema_bytes)
        }
    };
    generate_schema_tokens.to_tokens(&mut out);
    Ok(out.into())
}

#[cfg(not(feature = "build-schema"))]
#[proc_macro_attribute]
pub fn contract_state(_attr: TokenStream, item: TokenStream) -> TokenStream { item }

/// Derive the `SchemaType` trait for a type.
#[cfg(feature = "build-schema")]
#[proc_macro_derive(
    SchemaType,
    attributes(size_length, map_size_length, set_size_length, string_size_length)
)]
pub fn schema_type_derive(input: TokenStream) -> TokenStream {
    unwrap_or_report(schema_type_derive_worker(input))
}

#[cfg(feature = "build-schema")]
fn schema_type_derive_worker(input: TokenStream) -> syn::Result<TokenStream> {
    let ast: syn::DeriveInput = syn::parse(input)?;

    let data_name = &ast.ident;

    let body = match ast.data {
        syn::Data::Struct(ref data) => {
            let fields_tokens = schema_type_fields(&data.fields)?;
            quote! {
                concordium_std::schema::Type::Struct(#fields_tokens)
            }
        }
        syn::Data::Enum(ref data) => {
            let variant_tokens: Vec<_> = data
                .variants
                .iter()
                .map(|variant| {
                    let variant_name = &variant.ident.to_string();
                    let fields_tokens = schema_type_fields(&variant.fields)?;
                    Ok(quote! {
                        (String::from(#variant_name), #fields_tokens)
                    })
                })
                .collect::<syn::Result<_>>()?;
            quote! {
                concordium_std::schema::Type::Enum(vec! [ #(#variant_tokens),* ])
            }
        }
        _ => syn::Error::new(ast.span(), "Union is not supported").to_compile_error(),
    };

    let out = quote! {
        #[automatically_derived]
        impl concordium_std::schema::SchemaType for #data_name {
            fn get_type() -> concordium_std::schema::Type {
                #body
            }
        }
    };
    Ok(out.into())
}

#[cfg(not(feature = "build-schema"))]
#[proc_macro_derive(
    SchemaType,
    attributes(size_length, map_size_length, set_size_length, string_size_length)
)]
pub fn schema_type_derive(_input: TokenStream) -> TokenStream { TokenStream::new() }

#[cfg(feature = "build-schema")]
fn or_else_joined<A>(
    a: syn::Result<Option<A>>,
    b: impl FnOnce() -> syn::Result<Option<A>>,
) -> syn::Result<Option<A>> {
    match a {
        Ok(None) => b(),
        _ => a,
    }
}

#[cfg(feature = "build-schema")]
fn schema_type_field_type(field: &syn::Field) -> syn::Result<proc_macro2::TokenStream> {
    let field_type = &field.ty;
    if let Some(l) = or_else_joined(find_length_attribute(&field.attrs, "size_length"), || {
        or_else_joined(find_length_attribute(&field.attrs, "map_size_length"), || {
            or_else_joined(find_length_attribute(&field.attrs, "set_size_length"), || {
                find_length_attribute(&field.attrs, "string_size_length")
            })
        })
    })? {
        let size = format_ident!("U{}", 8 * l);
        Ok(quote! {
            <#field_type as concordium_std::schema::SchemaType>::get_type().set_size_length(concordium_std::schema::SizeLength::#size)
        })
    } else {
        Ok(quote! {
            <#field_type as concordium_std::schema::SchemaType>::get_type()
        })
    }
}

#[cfg(feature = "build-schema")]
fn schema_type_fields(fields: &syn::Fields) -> syn::Result<proc_macro2::TokenStream> {
    match fields {
        syn::Fields::Named(_) => {
            let fields_tokens: Vec<_> = fields
                .iter()
                .map(|field| {
                    let field_name = field.ident.clone().unwrap().to_string(); // safe since named fields
                    let field_schema_type = schema_type_field_type(&field)?;
                    Ok(quote! {
                        (String::from(#field_name), #field_schema_type)
                    })
                })
                .collect::<syn::Result<_>>()?;
            Ok(quote! { concordium_std::schema::Fields::Named(vec![ #(#fields_tokens),* ]) })
        }
        syn::Fields::Unnamed(_) => {
            let fields_tokens: Vec<_> =
                fields.iter().map(schema_type_field_type).collect::<syn::Result<_>>()?;
            Ok(quote! { concordium_std::schema::Fields::Unnamed(vec![ #(#fields_tokens),* ]) })
        }
        syn::Fields::Unit => Ok(quote! { concordium_std::schema::Fields::None }),
    }
}

#[proc_macro_attribute]
pub fn concordium_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    unwrap_or_report(concordium_test_worker(attr, item))
}

/// Derive the appropriate export for an annotated test function, when feature
/// "wasm-test" is enabled, otherwise behaves like `#[test]`.
#[cfg(feature = "wasm-test")]
fn concordium_test_worker(_attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let test_fn_ast: syn::ItemFn =
        attach_error(syn::parse(item), "#[concordium_test] can only be applied to functions.")?;

    let test_fn_name = &test_fn_ast.sig.ident;
    let rust_export_fn_name = format_ident!("concordium_test_{}", test_fn_name);
    let wasm_export_fn_name = format!("concordium_test {}", test_fn_name);

    let test_fn = quote! {
        // Setup test function
        #test_fn_ast

        // Export test function in wasm
        #[export_name = #wasm_export_fn_name]
        pub extern "C" fn #rust_export_fn_name() {
            #test_fn_name()
        }
    };
    Ok(test_fn.into())
}

/// Derive the appropriate export for an annotated test function, when feature
/// "wasm-test" is enabled, otherwise behaves like `#[test]`.
#[cfg(not(feature = "wasm-test"))]
fn concordium_test_worker(_attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let test_fn_ast: syn::ItemFn =
        attach_error(syn::parse(item), "#[concordium_test] can only be applied to functions.")?;

    let test_fn = quote! {
        #[test]
        #test_fn_ast
    };
    Ok(test_fn.into())
}

/// Sets the cfg for testing targeting either Wasm and native.
#[cfg(feature = "wasm-test")]
#[proc_macro_attribute]
pub fn concordium_cfg_test(_attr: TokenStream, item: TokenStream) -> TokenStream { item }

/// Sets the cfg for testing targeting either Wasm and native.
#[cfg(not(feature = "wasm-test"))]
#[proc_macro_attribute]
pub fn concordium_cfg_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = proc_macro2::TokenStream::from(item);
    let out = quote! {
        #[cfg(test)]
        #item
    };
    out.into()
}