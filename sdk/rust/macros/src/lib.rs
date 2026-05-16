/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use proc_macro_crate::{crate_name, FoundCrate};
use quote::{format_ident, quote};
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{
    parse_macro_input, parse_quote, Error, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn,
    ItemFn, ItemImpl, LitBool, LitStr, Path, PathArguments, Result, ReturnType, Token, Type,
};

#[proc_macro_derive(FlameMessage)]
pub fn derive_flame_message(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    expand_flame_message(input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro_attribute]
pub fn entrypoint(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as MacroArgs);
    let input = parse_macro_input!(input as ItemFn);
    expand_entrypoint(args, input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro_attribute]
pub fn instance(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as MacroArgs);
    let input = parse_macro_input!(input as ItemImpl);
    expand_instance(args, input)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[derive(Default)]
struct MacroArgs {
    crate_path: Option<Path>,
    entrypoint: Option<String>,
    enter: Option<String>,
    leave: Option<String>,
    debug: bool,
}

impl Parse for MacroArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut args = MacroArgs::default();

        while !input.is_empty() {
            let key = input.call(Ident::parse_any)?;

            if key == "debug" {
                if input.peek(Token![=]) {
                    input.parse::<Token![=]>()?;
                    args.debug = input.parse::<LitBool>()?.value;
                } else {
                    args.debug = true;
                }
            } else {
                input.parse::<Token![=]>()?;
                match key.to_string().as_str() {
                    "crate" => args.crate_path = Some(input.parse()?),
                    "entrypoint" => args.entrypoint = Some(input.parse::<LitStr>()?.value()),
                    "enter" => args.enter = Some(input.parse::<LitStr>()?.value()),
                    "leave" => args.leave = Some(input.parse::<LitStr>()?.value()),
                    _ => return Err(Error::new_spanned(key, "unsupported flame macro argument")),
                }
            }

            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        Ok(args)
    }
}

#[derive(Clone)]
enum HandlerInput {
    None,
    Required(Type),
    Optional(Type),
}

#[derive(Clone)]
enum HandlerOutput {
    Unit,
    Required,
    Optional,
}

struct HandlerSignature {
    has_instance_handle: bool,
    input: HandlerInput,
    output: HandlerOutput,
}

struct EnterHook {
    method: Ident,
    has_instance_handle: bool,
}

fn expand_flame_message(input: syn::DeriveInput) -> Result<TokenStream2> {
    let crate_path = flame_crate_path(&MacroArgs::default());
    let name = input.ident;
    let mut generics = input.generics;

    generics.make_where_clause().predicates.push(parse_quote!(
        Self: #crate_path::__private::serde::Serialize
            + #crate_path::__private::serde::de::DeserializeOwned
    ));

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics #crate_path::message::FlameMessage for #name #ty_generics #where_clause {
            fn encode(
                &self,
            ) -> ::std::result::Result<#crate_path::__private::bytes::Bytes, #crate_path::apis::FlameError> {
                #crate_path::__private::serde_json::to_vec(self)
                    .map(#crate_path::__private::bytes::Bytes::from)
                    .map_err(|err| #crate_path::apis::FlameError::Internal(err.to_string()))
            }

            fn decode(bytes: &[u8]) -> ::std::result::Result<Self, #crate_path::apis::FlameError> {
                #crate_path::__private::serde_json::from_slice(bytes)
                    .map_err(|err| #crate_path::apis::FlameError::InvalidConfig(err.to_string()))
            }
        }
    })
}

fn expand_entrypoint(args: MacroArgs, mut item: ItemFn) -> Result<TokenStream2> {
    if args.debug {
        return Err(Error::new(
            Span::call_site(),
            "debug mode is reserved for a future Flame Rust macro release",
        ));
    }

    let crate_path = flame_crate_path(&args);
    let handler = analyze_signature(&item.sig, false)?;
    let original_ident = item.sig.ident.clone();
    let impl_ident = format_ident!("__flame_{}_impl", original_ident);
    let wrapper_ident = format_ident!("__Flame_{}_Entrypoint", original_ident);
    item.sig.ident = impl_ident.clone();

    let vis = item.vis.clone();
    let task_body = task_invoke_body(
        &handler,
        quote!(#impl_ident),
        &crate_path,
        quote!(ctx.input),
    );

    Ok(quote! {
        #item

        #[allow(non_camel_case_types)]
        #vis struct #original_ident;

        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        #vis struct #wrapper_ident {
            session: ::std::sync::Mutex<::std::option::Option<#crate_path::service::SessionContext>>,
        }

        impl ::std::default::Default for #wrapper_ident {
            fn default() -> Self {
                Self {
                    session: ::std::sync::Mutex::new(None),
                }
            }
        }

        impl #wrapper_ident {
            fn set_session(
                &self,
                session: ::std::option::Option<#crate_path::service::SessionContext>,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                *self.session.lock().map_err(|_| {
                    #crate_path::apis::FlameError::Internal(
                        "flame instance session lock poisoned".to_string(),
                    )
                })? = session;
                Ok(())
            }

            fn flame_instance(
                &self,
            ) -> ::std::result::Result<#crate_path::service::FlameInstance, #crate_path::apis::FlameError> {
                let session = self.session.lock().map_err(|_| {
                    #crate_path::apis::FlameError::Internal(
                        "flame instance session lock poisoned".to_string(),
                    )
                })?
                .clone()
                .ok_or_else(|| {
                    #crate_path::apis::FlameError::InvalidConfig(
                        "flame instance is not bound to a session".to_string(),
                    )
                })?;

                Ok(#crate_path::service::FlameInstance::new(session))
            }
        }

        impl #crate_path::IntoFlameInstance for #original_ident {
            type Service = #wrapper_ident;

            fn into_flame_instance(self) -> Self::Service {
                #wrapper_ident::default()
            }
        }

        #[#crate_path::service::async_trait]
        impl #crate_path::service::FlameService for #wrapper_ident {
            async fn on_session_enter(
                &self,
                ctx: #crate_path::service::SessionContext,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                self.set_session(Some(ctx))
            }

            async fn on_task_invoke(
                &self,
                ctx: #crate_path::service::TaskContext,
            ) -> ::std::result::Result<
                ::std::option::Option<#crate_path::apis::TaskOutput>,
                #crate_path::apis::FlameError,
            > {
                #task_body
            }

            async fn on_session_leave(
                &self,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                self.set_session(None)
            }
        }
    })
}

fn expand_instance(args: MacroArgs, mut item: ItemImpl) -> Result<TokenStream2> {
    if args.debug {
        return Err(Error::new(
            Span::call_site(),
            "debug mode is reserved for a future Flame Rust macro release",
        ));
    }
    if item.trait_.is_some() {
        return Err(Error::new_spanned(
            item.impl_token,
            "#[flame_rs::instance] only supports inherent impl blocks",
        ));
    }

    let crate_path = flame_crate_path(&args);
    let self_ty = item.self_ty.as_ref().clone();
    let type_ident = self_type_ident(&self_ty)?;
    let wrapper_ident = format_ident!("__Flame_{}_Instance", type_ident);

    let mut entrypoint_methods = Vec::new();
    let configured_entrypoint = args.entrypoint.clone();
    let enter_name = args.enter.as_deref().unwrap_or("enter");
    let leave_name = args.leave.as_deref().unwrap_or("leave");
    let mut enter_hook = None;
    let mut leave_hook = None;

    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };

        let is_marker_entrypoint = has_entrypoint_attr(&method.attrs);
        if is_marker_entrypoint {
            entrypoint_methods.push(method.sig.ident.clone());
        }
        method.attrs.retain(|attr| !is_entrypoint_attr(attr.path()));

        if let Some(name) = &configured_entrypoint {
            if method.sig.ident == name.as_str() {
                entrypoint_methods.push(method.sig.ident.clone());
            }
        }
        if method.sig.ident == enter_name {
            enter_hook = Some(analyze_enter_hook(method)?);
        }
        if method.sig.ident == leave_name {
            analyze_leave_hook(method)?;
            leave_hook = Some(method.sig.ident.clone());
        }
    }

    entrypoint_methods.sort_by_key(|a| a.to_string());
    entrypoint_methods.dedup_by(|a, b| a == b);

    let entrypoint_ident = match entrypoint_methods.as_slice() {
        [ident] => ident.clone(),
        [] => {
            return Err(Error::new(
                Span::call_site(),
                "expected exactly one #[flame_rs::entrypoint] method",
            ))
        }
        _ => {
            return Err(Error::new(
                Span::call_site(),
                "expected exactly one entrypoint method",
            ))
        }
    };

    let entrypoint_method = item
        .items
        .iter()
        .find_map(|impl_item| match impl_item {
            ImplItem::Fn(method) if method.sig.ident == entrypoint_ident => Some(method),
            _ => None,
        })
        .ok_or_else(|| Error::new_spanned(&entrypoint_ident, "entrypoint method not found"))?;
    let handler = analyze_signature(&entrypoint_method.sig, true)?;
    let task_body = task_invoke_body(
        &handler,
        quote!(self.inner.#entrypoint_ident),
        &crate_path,
        quote!(ctx.input),
    );
    let enter_body = enter_body(enter_hook.as_ref());
    let leave_body = leave_body(leave_hook.as_ref());

    Ok(quote! {
        #item

        #[allow(non_camel_case_types)]
        #[doc(hidden)]
        pub struct #wrapper_ident {
            inner: #self_ty,
            session: ::std::sync::Mutex<::std::option::Option<#crate_path::service::SessionContext>>,
        }

        impl #wrapper_ident {
            fn set_session(
                &self,
                session: ::std::option::Option<#crate_path::service::SessionContext>,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                *self.session.lock().map_err(|_| {
                    #crate_path::apis::FlameError::Internal(
                        "flame instance session lock poisoned".to_string(),
                    )
                })? = session;
                Ok(())
            }

            fn flame_instance(
                &self,
            ) -> ::std::result::Result<#crate_path::service::FlameInstance, #crate_path::apis::FlameError> {
                let session = self.session.lock().map_err(|_| {
                    #crate_path::apis::FlameError::Internal(
                        "flame instance session lock poisoned".to_string(),
                    )
                })?
                .clone()
                .ok_or_else(|| {
                    #crate_path::apis::FlameError::InvalidConfig(
                        "flame instance is not bound to a session".to_string(),
                    )
                })?;

                Ok(#crate_path::service::FlameInstance::new(session))
            }
        }

        impl #crate_path::IntoFlameInstance for #self_ty {
            type Service = #wrapper_ident;

            fn into_flame_instance(self) -> Self::Service {
                #wrapper_ident {
                    inner: self,
                    session: ::std::sync::Mutex::new(None),
                }
            }
        }

        #[#crate_path::service::async_trait]
        impl #crate_path::service::FlameService for #wrapper_ident {
            async fn on_session_enter(
                &self,
                ctx: #crate_path::service::SessionContext,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                self.set_session(Some(ctx))?;
                #enter_body
            }

            async fn on_task_invoke(
                &self,
                ctx: #crate_path::service::TaskContext,
            ) -> ::std::result::Result<
                ::std::option::Option<#crate_path::apis::TaskOutput>,
                #crate_path::apis::FlameError,
            > {
                #task_body
            }

            async fn on_session_leave(
                &self,
            ) -> ::std::result::Result<(), #crate_path::apis::FlameError> {
                #leave_body
            }
        }
    })
}

fn task_invoke_body(
    handler: &HandlerSignature,
    callable: TokenStream2,
    crate_path: &TokenStream2,
    input_expr: TokenStream2,
) -> TokenStream2 {
    let mut prelude = Vec::new();
    let mut args = Vec::new();

    if handler.has_instance_handle {
        prelude.push(quote! {
            let __flame_instance = self.flame_instance()?;
        });
        args.push(quote!(__flame_instance));
    }

    match &handler.input {
        HandlerInput::None => {}
        HandlerInput::Required(ty) => {
            prelude.push(quote! {
                let __flame_input = #crate_path::message::decode_optional::<#ty>(#input_expr)?
                    .ok_or_else(|| {
                        #crate_path::apis::FlameError::InvalidConfig(
                            "missing task input".to_string(),
                        )
                    })?;
            });
            args.push(quote!(__flame_input));
        }
        HandlerInput::Optional(ty) => {
            prelude.push(quote! {
                let __flame_input = #crate_path::message::decode_optional::<#ty>(#input_expr)?;
            });
            args.push(quote!(__flame_input));
        }
    }

    match &handler.output {
        HandlerOutput::Unit => quote! {
            #(#prelude)*
            #callable(#(#args),*).await?;
            #crate_path::message::encode_unit()
        },
        HandlerOutput::Required => quote! {
            #(#prelude)*
            let __flame_output = #callable(#(#args),*).await?;
            #crate_path::message::encode(__flame_output)
        },
        HandlerOutput::Optional => quote! {
            #(#prelude)*
            let __flame_output = #callable(#(#args),*).await?;
            #crate_path::message::encode_optional(__flame_output)
        },
    }
}

fn enter_body(hook: Option<&EnterHook>) -> TokenStream2 {
    match hook {
        Some(hook) if hook.has_instance_handle => {
            let method = &hook.method;
            quote! {
                let __flame_instance = self.flame_instance()?;
                self.inner.#method(__flame_instance).await
            }
        }
        Some(hook) => {
            let method = &hook.method;
            quote!(self.inner.#method().await)
        }
        None => quote!(::std::result::Result::Ok(())),
    }
}

fn leave_body(hook: Option<&Ident>) -> TokenStream2 {
    match hook {
        Some(method) => quote! {
            let __flame_result = self.inner.#method().await;
            self.set_session(None)?;
            __flame_result
        },
        None => quote! {
            self.set_session(None)?;
            ::std::result::Result::Ok(())
        },
    }
}

fn analyze_signature(sig: &syn::Signature, expects_receiver: bool) -> Result<HandlerSignature> {
    if sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            sig.fn_token,
            "flame entrypoint must be async",
        ));
    }

    let mut args = sig.inputs.iter();
    if expects_receiver {
        match args.next() {
            Some(FnArg::Receiver(receiver))
                if receiver.reference.is_some() && receiver.mutability.is_none() => {}
            Some(arg) => {
                return Err(Error::new_spanned(
                    arg,
                    "flame instance methods must take &self",
                ))
            }
            None => {
                return Err(Error::new_spanned(
                    sig.ident.clone(),
                    "flame instance method must take &self",
                ))
            }
        }
    }

    let mut has_instance_handle = false;
    let mut input = HandlerInput::None;

    for arg in args {
        let FnArg::Typed(arg) = arg else {
            return Err(Error::new_spanned(arg, "unexpected self argument"));
        };

        let ty = arg.ty.as_ref();
        if is_type_named(ty, "FlameInstance") {
            if has_instance_handle || !matches!(input, HandlerInput::None) {
                return Err(Error::new_spanned(
                    ty,
                    "FlameInstance must be the first entrypoint argument",
                ));
            }
            has_instance_handle = true;
            continue;
        }

        if !matches!(input, HandlerInput::None) {
            return Err(Error::new_spanned(
                ty,
                "flame entrypoint supports at most one typed input",
            ));
        }
        reject_raw_type(ty)?;
        input = match extract_option_type(ty) {
            Some(inner) => {
                reject_raw_type(&inner)?;
                HandlerInput::Optional(inner)
            }
            None => HandlerInput::Required((*ty).clone()),
        };
    }

    let ok_ty = result_ok_type(&sig.output)?;
    reject_raw_type(&ok_ty)?;
    let output = if is_unit_type(&ok_ty) {
        HandlerOutput::Unit
    } else if let Some(inner) = extract_option_type(&ok_ty) {
        reject_raw_type(&inner)?;
        HandlerOutput::Optional
    } else {
        HandlerOutput::Required
    };

    Ok(HandlerSignature {
        has_instance_handle,
        input,
        output,
    })
}

fn analyze_enter_hook(method: &ImplItemFn) -> Result<EnterHook> {
    if method.sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            &method.sig.ident,
            "flame enter hook must be async",
        ));
    }
    ensure_result_unit(&method.sig.output)?;

    let mut args = method.sig.inputs.iter();
    match args.next() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_none() => {}
        Some(arg) => return Err(Error::new_spanned(arg, "flame enter hook must take &self")),
        None => {
            return Err(Error::new_spanned(
                &method.sig.ident,
                "flame enter hook must take &self",
            ))
        }
    }

    let has_instance_handle = match args.next() {
        Some(FnArg::Typed(arg)) if is_type_named(arg.ty.as_ref(), "FlameInstance") => true,
        Some(arg) => {
            return Err(Error::new_spanned(
                arg,
                "flame enter hook only supports an optional FlameInstance argument",
            ))
        }
        None => false,
    };

    if let Some(arg) = args.next() {
        return Err(Error::new_spanned(
            arg,
            "too many flame enter hook arguments",
        ));
    }

    Ok(EnterHook {
        method: method.sig.ident.clone(),
        has_instance_handle,
    })
}

fn analyze_leave_hook(method: &ImplItemFn) -> Result<()> {
    if method.sig.asyncness.is_none() {
        return Err(Error::new_spanned(
            &method.sig.ident,
            "flame leave hook must be async",
        ));
    }
    ensure_result_unit(&method.sig.output)?;

    let mut args = method.sig.inputs.iter();
    match args.next() {
        Some(FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_none() => {}
        Some(arg) => return Err(Error::new_spanned(arg, "flame leave hook must take &self")),
        None => {
            return Err(Error::new_spanned(
                &method.sig.ident,
                "flame leave hook must take &self",
            ))
        }
    }

    if let Some(arg) = args.next() {
        return Err(Error::new_spanned(
            arg,
            "flame leave hook takes no arguments",
        ));
    }

    Ok(())
}

fn result_ok_type(output: &ReturnType) -> Result<Type> {
    let ReturnType::Type(_, ty) = output else {
        return Err(Error::new(
            Span::call_site(),
            "flame functions must return Result<T, FlameError>",
        ));
    };

    let Type::Path(type_path) = ty.as_ref() else {
        return Err(Error::new_spanned(
            ty,
            "flame functions must return Result<T, FlameError>",
        ));
    };

    let Some(segment) = type_path.path.segments.last() else {
        return Err(Error::new_spanned(
            ty,
            "flame functions must return Result<T, FlameError>",
        ));
    };
    if segment.ident != "Result" {
        return Err(Error::new_spanned(
            ty,
            "flame functions must return Result<T, FlameError>",
        ));
    }

    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(Error::new_spanned(
            ty,
            "flame functions must return Result<T, FlameError>",
        ));
    };

    args.args
        .iter()
        .find_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .ok_or_else(|| Error::new_spanned(ty, "flame functions must return Result<T, FlameError>"))
}

fn ensure_result_unit(output: &ReturnType) -> Result<()> {
    let ok_ty = result_ok_type(output)?;
    if is_unit_type(&ok_ty) {
        Ok(())
    } else {
        Err(Error::new_spanned(
            ok_ty,
            "flame lifecycle hooks must return Result<(), FlameError>",
        ))
    }
}

fn extract_option_type(ty: &Type) -> Option<Type> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    let segment = type_path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    })
}

fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty())
}

fn is_type_named(ty: &Type, name: &str) -> bool {
    type_last_ident(ty).as_deref() == Some(name)
}

fn type_last_ident(ty: &Type) -> Option<String> {
    let Type::Path(type_path) = ty else {
        return None;
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn reject_raw_type(ty: &Type) -> Result<()> {
    if is_type_named(ty, "TaskInput") || is_type_named(ty, "TaskOutput") {
        return Err(Error::new_spanned(
            ty,
            "raw TaskInput and TaskOutput are not supported by high-level flame macros; implement FlameService directly for raw payloads",
        ));
    }
    Ok(())
}

fn self_type_ident(ty: &Type) -> Result<Ident> {
    let Type::Path(type_path) = ty else {
        return Err(Error::new_spanned(
            ty,
            "#[flame_rs::instance] requires a concrete struct type",
        ));
    };
    type_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| {
            Error::new_spanned(ty, "#[flame_rs::instance] requires a concrete struct type")
        })
}

fn has_entrypoint_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| is_entrypoint_attr(attr.path()))
}

fn is_entrypoint_attr(path: &Path) -> bool {
    path.segments
        .last()
        .map(|segment| segment.ident == "entrypoint")
        .unwrap_or(false)
}

fn flame_crate_path(args: &MacroArgs) -> TokenStream2 {
    if let Some(path) = &args.crate_path {
        return quote!(#path);
    }

    match crate_name("flame-rs") {
        Ok(FoundCrate::Itself) => quote!(crate),
        Ok(FoundCrate::Name(name)) => {
            let ident = Ident::new(&name, Span::call_site());
            quote!(#ident)
        }
        Err(_) => quote!(flame_rs),
    }
}
