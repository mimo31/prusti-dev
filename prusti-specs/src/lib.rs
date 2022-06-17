#![deny(unused_must_use)]
#![feature(drain_filter)]
#![feature(box_patterns)]
#![feature(box_syntax)]
#![feature(if_let_guard)]
// This Clippy chcek seems to be always wrong.
#![allow(clippy::iter_with_drain)]

#[macro_use]
mod common;
mod extern_spec_rewriter;
mod ghost_constraints;
mod parse_closure_macro;
mod parse_quote_spanned;
mod predicate;
mod rewriter;
mod span_overrider;
mod spec_attribute_kind;
pub mod specifications;
mod type_model;
mod user_provided_type_params;
mod print_counterexample;

use syn::{punctuated::Punctuated, parse::Parser, Expr, Token, Pat, PatLit, ExprLit, Lit, token::Token, Fields};
use log::{error};
use proc_macro2::{Span, TokenStream, TokenTree, Punct};
use quote::{quote_spanned, ToTokens};
use rewriter::AstRewriter;
use std::convert::TryInto;
use syn::spanned::Spanned;
use itertools::Itertools;

use crate::{
    common::{merge_generics, RewritableReceiver, SelfTypeRewriter},
    predicate::{is_predicate_macro, ParsedPredicate},
    specifications::preparser::{parse_ghost_constraint, parse_prusti, NestedSpec},
};
pub use extern_spec_rewriter::ExternSpecKind;
use parse_closure_macro::ClosureWithSpec;
use prusti_utils::force_matches;
pub use spec_attribute_kind::SpecAttributeKind;
use specifications::{common::SpecificationId, untyped};

macro_rules! handle_result {
    ($parse_result: expr) => {
        match $parse_result {
            Ok(data) => data,
            Err(err) => return err.to_compile_error(),
        }
    };
}

fn extract_prusti_attributes(
    item: &mut untyped::AnyFnItem,
) -> Vec<(SpecAttributeKind, TokenStream)> {
    let mut prusti_attributes = Vec::new();
    let mut regular_attributes = Vec::new();
    for attr in item.attrs_mut().drain(0..) {
        if attr.path.segments.len() == 1 {
            if let Ok(attr_kind) = attr.path.segments[0].ident.to_string().try_into() {
                let tokens = match attr_kind {
                    SpecAttributeKind::Requires
                    | SpecAttributeKind::Ensures
                    | SpecAttributeKind::AfterExpiry
                    | SpecAttributeKind::AssertOnExpiry
                    | SpecAttributeKind::GhostConstraint => {
                        // We need to drop the surrounding parenthesis to make the
                        // tokens identical to the ones passed by the native procedural
                        // macro call.
                        let mut iter = attr.tokens.into_iter();
                        let tokens = force_matches!(iter.next().unwrap(), TokenTree::Group(group) => group.stream());
                        assert!(iter.next().is_none(), "Unexpected shape of an attribute.");
                        tokens
                    }
                    // Nothing to do for attributes without arguments.
                    SpecAttributeKind::Pure
                    | SpecAttributeKind::Trusted
                    | SpecAttributeKind::Predicate => {
                        assert!(attr.tokens.is_empty(), "Unexpected shape of an attribute.");
                        attr.tokens
                    }
                    SpecAttributeKind::Invariant => unreachable!("type invariant on function"),
                };
                prusti_attributes.push((attr_kind, tokens));
            } else {
                regular_attributes.push(attr);
            }
        } else {
            regular_attributes.push(attr);
        }
    }
    *item.attrs_mut() = regular_attributes;
    prusti_attributes
}

/// Rewrite an item as required by *all* its specification attributes.
///
/// The first attribute (the outer one) needs to be passed via `attr_kind` and `attr` because
/// the compiler executes it as as a procedural macro attribute.
pub fn rewrite_prusti_attributes(
    outer_attr_kind: SpecAttributeKind,
    outer_attr_tokens: TokenStream,
    item_tokens: TokenStream,
) -> TokenStream {
    let mut item: untyped::AnyFnItem = handle_result!(syn::parse2(item_tokens));

    // Start with the outer attribute
    let mut prusti_attributes = vec![(outer_attr_kind, outer_attr_tokens)];

    // Collect the remaining Prusti attributes, removing them from `item`.
    prusti_attributes.extend(extract_prusti_attributes(&mut item));

    // make sure to also update the check in the predicate! handling method
    if prusti_attributes
        .iter()
        .any(|(ak, _)| ak == &SpecAttributeKind::Predicate)
    {
        return syn::Error::new(
            item.span(),
            "`predicate!` is incompatible with other Prusti attributes",
        )
        .to_compile_error();
    }

    let (generated_spec_items, generated_attributes) =
        handle_result!(generate_spec_and_assertions(prusti_attributes, &item));

    quote_spanned! {item.span()=>
        #(#generated_spec_items)*
        #(#generated_attributes)*
        #item
    }
}

type GeneratedResult = syn::Result<(Vec<syn::Item>, Vec<syn::Attribute>)>;

/// Generate spec items and attributes for `item` from the Prusti attributes
fn generate_spec_and_assertions(
    mut prusti_attributes: Vec<(SpecAttributeKind, TokenStream)>,
    item: &untyped::AnyFnItem,
) -> GeneratedResult {
    let mut generated_items = vec![];
    let mut generated_attributes = vec![];

    for (attr_kind, attr_tokens) in prusti_attributes.drain(..) {
        let rewriting_result = match attr_kind {
            SpecAttributeKind::Requires => generate_for_requires(attr_tokens, item),
            SpecAttributeKind::Ensures => generate_for_ensures(attr_tokens, item),
            SpecAttributeKind::AfterExpiry => generate_for_after_expiry(attr_tokens, item),
            SpecAttributeKind::AssertOnExpiry => generate_for_assert_on_expiry(attr_tokens, item),
            SpecAttributeKind::Pure => generate_for_pure(attr_tokens, item),
            SpecAttributeKind::Trusted => generate_for_trusted(attr_tokens, item),
            // Predicates are handled separately below; the entry in the SpecAttributeKind enum
            // only exists so we successfully parse it and emit an error in
            // `check_incompatible_attrs`; so we'll never reach here.
            SpecAttributeKind::Predicate => unreachable!(),
            SpecAttributeKind::Invariant => unreachable!(),
            SpecAttributeKind::GhostConstraint => ghost_constraints::generate(attr_tokens, item),
        };
        let (new_items, new_attributes) = rewriting_result?;
        generated_items.extend(new_items);
        generated_attributes.extend(new_attributes);
    }

    Ok((generated_items, generated_attributes))
}

/// Generate spec items and attributes to typecheck the and later retrieve "requires" annotations.
fn generate_for_requires(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id = rewriter.generate_spec_id();
    let spec_id_str = spec_id.to_string();
    let spec_item =
        rewriter.process_assertion(rewriter::SpecItemType::Precondition, spec_id, attr, item)?;
    Ok((
        vec![spec_item],
        vec![parse_quote_spanned! {item.span()=>
            #[prusti::pre_spec_id_ref = #spec_id_str]
        }],
    ))
}

/// Generate spec items and attributes to typecheck the and later retrieve "ensures" annotations.
fn generate_for_ensures(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id = rewriter.generate_spec_id();
    let spec_id_str = spec_id.to_string();
    let spec_item =
        rewriter.process_assertion(rewriter::SpecItemType::Postcondition, spec_id, attr, item)?;
    Ok((
        vec![spec_item],
        vec![parse_quote_spanned! {item.span()=>
            #[prusti::post_spec_id_ref = #spec_id_str]
        }],
    ))
}

/// Generate spec items and attributes to typecheck and later retrieve "after_expiry" annotations.
fn generate_for_after_expiry(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id = rewriter.generate_spec_id();
    let spec_id_str = spec_id.to_string();
    let spec_item = rewriter.process_pledge(spec_id, attr, item)?;
    Ok((
        vec![spec_item],
        vec![parse_quote_spanned! {item.span()=>
            #[prusti::pledge_spec_id_ref = #spec_id_str]
        }],
    ))
}

/// Generate spec items and attributes to typecheck and later retrieve "after_expiry" annotations.
fn generate_for_assert_on_expiry(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id_lhs = rewriter.generate_spec_id();
    let spec_id_lhs_str = spec_id_lhs.to_string();
    let spec_id_rhs = rewriter.generate_spec_id();
    let spec_id_rhs_str = spec_id_rhs.to_string();
    let (spec_item_lhs, spec_item_rhs) =
        rewriter.process_assert_pledge(spec_id_lhs, spec_id_rhs, attr, item)?;
    Ok((
        vec![spec_item_lhs, spec_item_rhs],
        vec![
            parse_quote_spanned! {item.span()=>
                #[prusti::assert_pledge_spec_id_ref_lhs = #spec_id_lhs_str]
            },
            parse_quote_spanned! {item.span()=>
                #[prusti::assert_pledge_spec_id_ref_rhs = #spec_id_rhs_str]
            },
        ],
    ))
}

/// Generate spec items and attributes to typecheck and later retrieve "pure" annotations.
fn generate_for_pure(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    if !attr.is_empty() {
        return Err(syn::Error::new(
            attr.span(),
            "the `#[pure]` attribute does not take parameters",
        ));
    }

    Ok((
        vec![],
        vec![parse_quote_spanned! {item.span()=>
            #[prusti::pure]
        }],
    ))
}

/// Generate spec items and attributes to typecheck and later retrieve "trusted" annotations.
fn generate_for_trusted(attr: TokenStream, item: &untyped::AnyFnItem) -> GeneratedResult {
    if !attr.is_empty() {
        return Err(syn::Error::new(
            attr.span(),
            "the `#[trusted]` attribute does not take parameters",
        ));
    }

    Ok((
        vec![],
        vec![parse_quote_spanned! {item.span()=>
            #[prusti::trusted]
        }],
    ))
}

pub fn body_invariant(tokens: TokenStream) -> TokenStream {
    generate_expression_closure(&AstRewriter::process_loop_invariant, tokens)
}

pub fn prusti_assertion(tokens: TokenStream) -> TokenStream {
    generate_expression_closure(&AstRewriter::process_prusti_assertion, tokens)
}

pub fn prusti_assume(tokens: TokenStream) -> TokenStream {
    generate_expression_closure(&AstRewriter::process_prusti_assumption, tokens)
}

/// Generates the TokenStream encoding an expression using prusti syntax
/// Used for body invariants, assertions, and assumptions
fn generate_expression_closure(
    fun: &dyn Fn(&mut AstRewriter, SpecificationId, TokenStream) -> syn::Result<TokenStream>,
    tokens: TokenStream,
) -> TokenStream {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id = rewriter.generate_spec_id();
    let closure = handle_result!(fun(&mut rewriter, spec_id, tokens));
    let callsite_span = Span::call_site();
    quote_spanned! {callsite_span=>
        #[allow(unused_must_use, unused_variables, unused_braces, unused_parens)]
        if false {
            #closure
        }
    }
}

/// Unlike the functions above, which are only called from
/// prusti-contracts-internal, this function also needs to be called
/// from prusti-contracts-impl, because we still need to parse the
/// macro in order to replace it with the closure definition.
/// Therefore, there is an extra parameter drop_spec here which tells
/// the function whether to keep the specification (for -internal) or
/// drop it (for -impl).
pub fn closure(tokens: TokenStream, drop_spec: bool) -> TokenStream {
    let cl_spec: ClosureWithSpec = handle_result!(syn::parse(tokens.into()));
    let callsite_span = Span::call_site();

    if drop_spec {
        return cl_spec.cl.into_token_stream();
    }

    let mut rewriter = rewriter::AstRewriter::new();

    let mut preconds: Vec<(SpecificationId, syn::Expr)> = vec![];
    let mut postconds: Vec<(SpecificationId, syn::Expr)> = vec![];

    let mut cl_annotations = TokenStream::new();

    for r in cl_spec.pres {
        let spec_id = rewriter.generate_spec_id();
        let precond =
            handle_result!(rewriter.process_closure_assertion(spec_id, r.to_token_stream(),));
        preconds.push((spec_id, precond));
        let spec_id_str = spec_id.to_string();
        cl_annotations.extend(quote_spanned! {callsite_span=>
            #[prusti::pre_spec_id_ref = #spec_id_str]
        });
    }

    for e in cl_spec.posts {
        let spec_id = rewriter.generate_spec_id();
        let postcond =
            handle_result!(rewriter.process_closure_assertion(spec_id, e.to_token_stream(),));
        postconds.push((spec_id, postcond));
        let spec_id_str = spec_id.to_string();
        cl_annotations.extend(quote_spanned! {callsite_span=>
            #[prusti::post_spec_id_ref = #spec_id_str]
        });
    }

    let syn::ExprClosure {
        attrs,
        asyncness,
        movability,
        capture,
        or1_token,
        inputs,
        or2_token,
        output,
        body,
    } = cl_spec.cl;

    let output_type: syn::Type = match output {
        syn::ReturnType::Default => {
            return syn::Error::new(output.span(), "closure must specify return type")
                .to_compile_error();
        }
        syn::ReturnType::Type(_, ref ty) => (**ty).clone(),
    };

    let (spec_toks_pre, spec_toks_post) =
        handle_result!(rewriter.process_closure(inputs.clone(), output_type, preconds, postconds,));

    let mut attrs_ts = TokenStream::new();
    for a in attrs {
        attrs_ts.extend(a.into_token_stream());
    }

    quote_spanned! {callsite_span=>
        {
            #[allow(unused_variables, unused_braces, unused_parens)]
            #[prusti::closure]
            #cl_annotations #attrs_ts
            let _prusti_closure =
                #asyncness #movability #capture
                #or1_token #inputs #or2_token #output
                {
                    #[allow(unused_must_use, unused_braces, unused_parens)]
                    if false {
                        #spec_toks_pre
                    }
                    let result = #body ;
                    #[allow(unused_must_use, unused_braces, unused_parens)]
                    if false {
                        #spec_toks_post
                    }
                    result
                };
            _prusti_closure
        }
    }
}

pub fn refine_trait_spec(_attr: TokenStream, tokens: TokenStream) -> TokenStream {
    let mut impl_block: syn::ItemImpl = handle_result!(syn::parse2(tokens));
    let impl_generics = &impl_block.generics;

    let trait_path: syn::TypePath = match &impl_block.trait_ {
        Some((_, trait_path, _)) => parse_quote_spanned!(trait_path.span()=>#trait_path),
        None => handle_result!(Err(syn::Error::new(
            impl_block.span(),
            "Can refine trait specifications only on trait implementation blocks"
        ))),
    };

    let self_type_path: &syn::TypePath = match &*impl_block.self_ty {
        syn::Type::Path(type_path) => type_path,
        _ => unimplemented!("Currently not supported: {:?}", impl_block.self_ty),
    };

    let mut new_items = Vec::new();
    let mut generated_spec_items = Vec::new();
    for item in impl_block.items {
        match item {
            syn::ImplItem::Method(method) => {
                let mut method_item = untyped::AnyFnItem::ImplMethod(method);
                let prusti_attributes: Vec<_> = extract_prusti_attributes(&mut method_item);

                let illegal_attribute_span = prusti_attributes
                    .iter()
                    .filter(|(kind, _)| kind == &SpecAttributeKind::GhostConstraint)
                    .map(|(_, tokens)| tokens.span())
                    .next();
                if let Some(span) = illegal_attribute_span {
                    let err = Err(syn::Error::new(
                        span,
                        "Ghost constraints in trait spec refinements not supported",
                    ));
                    handle_result!(err);
                }

                let (spec_items, generated_attributes) = handle_result!(
                    generate_spec_and_assertions(prusti_attributes, &method_item)
                );

                spec_items
                    .into_iter()
                    .map(|spec_item| match spec_item {
                        syn::Item::Fn(spec_item_fn) => spec_item_fn,
                        x => unimplemented!("Unexpected variant: {:?}", x),
                    })
                    .for_each(|spec_item_fn| generated_spec_items.push(spec_item_fn));

                let new_item = parse_quote_spanned! {method_item.span()=>
                    #(#generated_attributes)*
                    #method_item
                };
                new_items.push(new_item);
            }
            syn::ImplItem::Macro(makro) if is_predicate_macro(&makro) => {
                let parsed_predicate =
                    handle_result!(predicate::parse_predicate_in_impl(makro.mac.tokens.clone()));

                let predicate = force_matches!(parsed_predicate, ParsedPredicate::Impl(p) => p);

                // Patch spec function: Rewrite self with _self: <SpecStruct>
                let spec_function = force_matches!(predicate.spec_function,
                    syn::Item::Fn(item_fn) => item_fn);
                generated_spec_items.push(spec_function);

                // Add patched predicate function to new items
                new_items.push(syn::ImplItem::Method(predicate.patched_function));
            }
            _ => new_items.push(item),
        }
    }

    // Patch the spec items (merge generics, handle associated types, rewrite receiver)
    for generated_spec_item in generated_spec_items.iter_mut() {
        merge_generics(&mut generated_spec_item.sig.generics, impl_generics);
        generated_spec_item.rewrite_self_type(self_type_path, Some(&trait_path));
        generated_spec_item.rewrite_receiver(self_type_path);
    }

    impl_block.items = new_items;
    quote_spanned! {impl_block.span()=>
        #(#generated_spec_items)*
        #impl_block
    }
}

pub fn trusted(attr: TokenStream, tokens: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            attr.span(),
            "the `#[trusted]` attribute does not take parameters",
        )
        .to_compile_error();
    }

    // `#[trusted]` can be applied to both types and to methods, figure out
    // which one by trying to parse a `DeriveInput`.
    if syn::parse2::<syn::DeriveInput>(tokens.clone()).is_ok() {
        // TODO: reduce duplication with `invariant`
        let mut rewriter = rewriter::AstRewriter::new();
        let spec_id = rewriter.generate_spec_id();
        let spec_id_str = spec_id.to_string();

        let item: syn::DeriveInput = handle_result!(syn::parse2(tokens));
        let item_span = item.span();
        let item_ident = item.ident.clone();
        let item_name = syn::Ident::new(
            &format!("prusti_trusted_item_{}_{}", item_ident, spec_id),
            item_span,
        );

        let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
            #[allow(unused_variables, dead_code, non_snake_case)]
            #[prusti::spec_only]
            #[prusti::trusted_type]
            #[prusti::spec_id = #spec_id_str]
            fn #item_name(self) {}
        };

        let generics = &item.generics;
        let generics_idents = generics
            .params
            .iter()
            .filter_map(|generic_param| match generic_param {
                syn::GenericParam::Type(type_param) => Some(type_param.ident.clone()),
                _ => None,
            })
            .collect::<syn::punctuated::Punctuated<_, syn::Token![,]>>();
        // TODO: similarly to extern_specs, don't generate an actual impl
        let item_impl: syn::ItemImpl = parse_quote_spanned! {item_span=>
            impl #generics #item_ident <#generics_idents> {
                #spec_item
            }
        };
        quote_spanned! { item_span =>
            #item
            #item_impl
        }
    } else {
        rewrite_prusti_attributes(SpecAttributeKind::Trusted, attr, tokens)
    }
}

pub fn invariant(attr: TokenStream, tokens: TokenStream) -> TokenStream {
    let mut rewriter = rewriter::AstRewriter::new();
    let spec_id = rewriter.generate_spec_id();
    let spec_id_str = spec_id.to_string();

    let item: syn::DeriveInput = handle_result!(syn::parse2(tokens));
    let item_span = item.span();
    let item_ident = item.ident.clone();
    let item_name = syn::Ident::new(
        &format!("prusti_invariant_item_{}_{}", item_ident, spec_id),
        item_span,
    );

    let attr = handle_result!(parse_prusti(attr));

    // TODO: move some of this to AstRewriter?
    // see AstRewriter::generate_spec_item_fn for explanation of syntax below
    let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
        #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case)]
        #[prusti::spec_only]
        #[prusti::type_invariant_spec]
        #[prusti::spec_id = #spec_id_str]
        fn #item_name(self) -> bool {
            !!((#attr) : bool)
        }
    };

    let generics = item.generics.clone();
    let generics_idents = generics
        .params
        .iter()
        .filter_map(|generic_param| match generic_param {
            syn::GenericParam::Type(type_param) => Some(type_param.ident.clone()),
            _ => None,
        })
        .collect::<syn::punctuated::Punctuated<_, syn::Token![,]>>();
    // TODO: similarly to extern_specs, don't generate an actual impl
    let item_impl: syn::ItemImpl = parse_quote_spanned! {item_span=>
        impl #generics #item_ident < #generics_idents > {
            #spec_item
        }
    };
    quote_spanned! { item_span =>
        #item
        #item_impl
    }
}

pub fn extern_spec(attr: TokenStream, tokens: TokenStream) -> TokenStream {
    let item: syn::Item = handle_result!(syn::parse2(tokens));
    match item {
        syn::Item::Impl(item_impl) => {
            handle_result!(extern_spec_rewriter::impls::rewrite_extern_spec(&item_impl))
        }
        syn::Item::Trait(item_trait) => {
            handle_result!(extern_spec_rewriter::traits::rewrite_extern_spec(
                &item_trait
            ))
        }
        syn::Item::Mod(mut item_mod) => {
            handle_result!(extern_spec_rewriter::mods::rewrite_extern_spec(
                &mut item_mod
            ))
        }
        _ => syn::Error::new(attr.span(), "Extern specs cannot be attached to this item")
            .to_compile_error(),
    }
}

pub fn predicate(tokens: TokenStream) -> TokenStream {
    let parsed = handle_result!(predicate::parse_predicate(tokens));
    parsed.into_token_stream()
}

pub fn type_model(attr: TokenStream, tokens: TokenStream) -> TokenStream {
    let _ = env_logger::try_init();
    let item: syn::Item = handle_result!(syn::parse2(tokens));
    match item {
        syn::Item::Struct(item_struct) => {
            handle_result!(type_model::rewrite(item_struct))
        }
        _ => syn::Error::new(
            attr.span(),
            "Only structs can be attributed with a type model",
        )
        .to_compile_error(),
    }
}

pub fn print_counterexample(attr: TokenStream, tokens: TokenStream) -> TokenStream {
    //TODO rewrite error messages such that the apper for al arguments at once
    //TODO check for multiple print_counterexample, it should be allowed only once, should be fine
    
    let _ = env_logger::try_init();
    let tokens_clone= tokens.clone();
    let item: syn::Item = handle_result!(syn::parse2(tokens));
    let item2 = item.clone();
    
    //let attr_parsed: syn::Item = handle_result!(syn::parse2(attr2));
    //error!("print parsed attr: {:?}", attr_parsed);
    
    
    error!("type of struct: {:?}", item);
    
    let spec_item = match item {
        syn::Item::Struct(item_struct) => {
            error!("counterexample print other attriutes: {:#?}", item_struct.attrs);
            //check if type is a model
            if let Some(_) = item_struct.attrs.iter().find( |attr| attr.path.get_ident().and_then(| x | Some(x.to_string())) == Some("model".to_string())){
                let parser = Punctuated::<Pat, Token![,]>::parse_terminated; //parse_separated_nonempty;
                let item_span = item_struct.span();
                let spec_item: syn::Item = parse_quote_spanned! {item_span=>
                    #[print_counterexample(#attr)]
                    #item_struct
                };
                match &spec_item{
                    syn::Item::Struct(tmp) => error!("print new struct: {:#?}", &tmp.attrs),
                    _ => (),
                }
                
                return type_model(TokenStream::new(), spec_item.into_token_stream());
            }
            error!("print attr: {}", attr);
            error!("print attr: {:?}", attr);
            //let parser = syn::Attribute::parse_outer;
            let parser = Punctuated::<Pat, Token![,]>::parse_terminated; //parse_separated_nonempty;
            let attrs = handle_result!(parser.parse(attr.clone().into()));
            let attrs2 = attrs.clone();
            let length = attrs.len();
            let callsite_span = Span::call_site();
            let mut attrs_iter = attrs.into_iter();
            let first_arg = if let Some(text) = attrs_iter.next(){
                let span = text.span();
                error!("text node: {:?}", text);
                match text {
                    Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Str(lit_str) }) }) => {
                        let value = lit_str.value();
                        error!("value of text node: {}", value);
                        let count = value.matches("{}").count();
                        error!("count of {{}} in text node: {}", count);
                        if count != length-1{
                            return syn::Error::new(
                                span,
                                "number of arguments and number of {} do not match",
                            )
                            .to_compile_error().into_token_stream();
                        }
                        quote_spanned! {callsite_span=> #value;}
                    },
                    _ => return syn::Error::new(
                        span,
                        "first argument of custom print must be a string literal",
                    )
                    .to_compile_error().into_token_stream(),
                }
            }else {
                return syn::Error::new(
                    attr.span(),
                    "print_counterexample expects at least one argument for struct",
                )
                .to_compile_error().into_token_stream();
            };

            
            let args = attrs_iter.map(|pat | {
                match pat {
                    Pat::Ident(pat_ident) => {
                        quote_spanned! {callsite_span=> #pat_ident; }
                    },
                    Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Int(lit_int)})}) => {
                        quote_spanned! {callsite_span=> #lit_int; }
                    },
                    _ => {error!("variable node {:?}", pat);
                        syn::Error::new(
                        pat.span(),
                        "argument must be a name or an integer",
                    )
                    .to_compile_error().into_token_stream()},
                }
            }).collect::<TokenStream>();

            error!("print args: {}", args);
            error!("print args: {:?}", args);
            //error!("parsed attr: {:?}", attrs);
            let callsite_span = Span::call_site();
            //let attrs2 = attrs.into_iter().map(|a|  Punctuated::new(a, Token![;])).collect::<Punctuated<Pat, Token![,]>>();
            //let attrs2 = attrs.into_iter().skip(1).collect::<Punctuated<Pat, Token![,]>>(); //map(|(a , b) |       ).collect::<Punctuated<Pat, Token![,]>>();
            /*let attrs2 = attrs.into_iter().map(| a |{  let name = 
                match a {
                    Pat::Ident(PatIdent) => PatIdent.ident,
                    Pat::Lit(PatLit) => ,
                    _ => "",
                }
                
                
                a.ident.as_ref().unwrap().clone(); let typ = a.ty.clone(); quote_spanned! {callsite_span=> let #name: #typ = self.#name; }}).collect::<TokenStream>();
            
        */

            /*let result = if is_post && !attrs.empty_or_trailing() {
                quote_spanned! {callsite_span=> , result: #output }
            } else if is_post {
                quote_spanned! {callsite_span=> result: #output }
            } else {
                TokenStream::new()
            };*/

            //let attr2: ParseBuffer = attr.into(); // handle_result!(syn::parse(attr.into()));
            //let mut attrs = handle_result!(syn::parse2(attr2.into() as ParseStream)); //.into().call(syn::Attribute::parse_outer));
            //let attrs: Vec<syn::Attribute> = handle_result!(attr.call(syn::Attribute::parse_outer));
            //let attrs: Vec<syn::Attribute> = handle_result!(syn::parse2(attr)).call(syn::Attribute::parse_outer);
            //let attrs: Punctuated<Expr, Token![,]> = handle_result!(syn::parse2(attr));
            //error!("parsed attr: {:?}", attrs2);


            
            let mut rewriter = rewriter::AstRewriter::new();
            let spec_id = rewriter.generate_spec_id();
            let spec_id_str = spec_id.to_string();
            error!("print spec_id: {:?}", spec_id);
            let item_struct2 = item_struct.clone();
            let item_span = item_struct.span();
            error!("print span: {:?}", item_span);
            //let type = syn:
            let item_name = syn::Ident::new(
                &format!("prusti_print_counterexample_item_{}_{}", item_struct.ident, spec_id),
                item_span,
            );

            //let callsite_span = Span::call_site();
            /*let test = match item_struct.fields{
                syn::Fields::Named(ref fields_named) => fields_named.named.iter().map(| a |{  let name = a.ident.as_ref().unwrap().clone(); let typ = a.ty.clone(); quote_spanned! {callsite_span=> #name: #typ, }}).collect::<TokenStream>(), 
                _ => TokenStream::new(),//fields_named.names.iter().map(| (a, b) |  {let name = a.itent; let typ = a.typ; quote_spanned! {callsite_span=> , #name: #typ }}).collect(),
                /*Unnamed(fields_unnamed) => (),
                Unit => (),*/
            };*/
            //error!("print params: {:?}", test);
            let mut args2: Punctuated<Pat, Token![,]> = attrs2.into_iter().skip(1).unique().collect::<Punctuated<Pat, Token![,]>>(); //TODO skip duplicate
            //add trailing punctuation
            if !args2.empty_or_trailing(){
                args2.push_punct(<syn::Token![,]>::default());
            }
            //let typ = Token![item_struct];
            //let format = format!("format!");
            //tmp : #item_struct.ident
            //tmp: #typ
            error!("print item_name: {:?}", item_name);

            let typ = item_struct.ident.clone();

            let spec_item = match item_struct.fields{
                Fields::Named(ref fields_named) => {
                    let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                        #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                        #[prusti::spec_only]
                        #[prusti::counterexample_print]
                        #[prusti::spec_id = #spec_id_str]
                        fn #item_name(self){
                            if let #typ{#args2 ..} = self{
                                #first_arg
                                #args
                            }
                        }
                    };
                    spec_item
                },
                Fields::Unnamed(ref fields_unnamed) => {
                    
                    //check if all args are possible
                    for arg in &args2{
                        if let Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Int(lit_int)})}) = arg{
                            let value:u32 = lit_int.base10_parse().ok().unwrap(); //TODO find a better solution //can only be positive //why does handle_resutl not work
                            error!("print value: {}", value);
                            if value >= fields_unnamed.unnamed.len() as u32 {
                                return syn::Error::new(
                                    arg.span(),
                                    format!("struct `{}` does not have a field named {}", item_struct.ident, value),
                                )
                                .to_compile_error().into_token_stream();
                            }
                        } else {
                            return syn::Error::new(
                                arg.span(),
                                format!("struct `{}` needs integer as arguments", item_struct.ident),
                            )
                            .to_compile_error().into_token_stream();
                        }
                    }
                    
                    let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                        #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                        #[prusti::spec_only]
                        #[prusti::counterexample_print]
                        #[prusti::spec_id = #spec_id_str]
                        fn #item_name(self){
                            if let #typ{..} = self{
                                #first_arg
                                #args
                            }
                        }
                    };
                    spec_item
                },
                Fields::Unit => {
                    if length == 1{
                        let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                            #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                            #[prusti::spec_only]
                            #[prusti::counterexample_print]
                            #[prusti::spec_id = #spec_id_str]
                            fn #item_name(self){
                                if let #typ{..} = self{
                                    #first_arg
                                }
                            }
                        };
                        spec_item
                    } else {
                        return syn::Error::new(
                            attr.span(),
                            format!("struct `{}` expects exactly one argument", item_struct.ident),
                        )
                        .to_compile_error().into_token_stream();
                    }
                },
            };
            /*#[print_counterexampe("test", 0, 1)]
            enum X{
                #[print_counterexampe("test", 0, 1)]
                f(i32),
                g(i32, i32),
            }*/
            /*fn #item_name(self, #test ) {
                    format!(#attr);
                }*/
            //error!("print fuction: {:?}", spec_item);
            //let tmp = syn::Item::Fn(spec_item).into_token_stream();
            //error!("print function: {}", tmp);
            //tmp

            let generics = &item_struct.generics;
            let generics_idents = generics
                .params
                .iter()
                .filter_map(|generic_param| match generic_param {
                    syn::GenericParam::Type(type_param) => Some(type_param.ident.clone()),
                    _ => None,
                })
                .collect::<syn::punctuated::Punctuated<_, syn::Token![,]>>();
            let item_impl: syn::ItemImpl = parse_quote_spanned! {item_span=>
                impl #generics #typ <#generics_idents> {
                    #spec_item
                }
            };
            let tmp = quote_spanned! { item_span =>
                #item_struct2
                #item_impl
            };
            error!("print impl: {}", tmp);
            tmp
        }
        syn::Item::Enum(item_enum) => {
            let mut item_enum2 = item_enum.clone();
            //remove all macros inside the enum
            for variant in &mut item_enum2.variants{
                variant.attrs.retain( |attr| attr.path.get_ident().and_then(| x | Some(x.to_string())) != Some("print_counterexample".to_string()));
            }

            error!("print attr: {}", attr);
            error!("print attr: {:?}", attr);
            //let parser = syn::Attribute::parse_outer;
            let parser = Punctuated::<Pat, Token![,]>::parse_terminated; //parse_separated_nonempty;
            let attrs = handle_result!(parser.parse(attr.clone().into()));
            let length = attrs.len();
            if length != 0{
                return syn::Error::new(
                    attr.span(),
                    "Custom counterexample print for enum should not have an argument",
                )
                .to_compile_error();
            }
            let mut spec_items:Vec<syn::ItemFn> = vec![]; 
            for variant in item_enum.variants{
                error!("print variant: {:?}", variant);
                if let Some(custom_print) = variant.attrs.into_iter().find( |attr| attr.path.get_ident().and_then(| x | Some(x.to_string())) == Some("print_counterexample".to_string())){
                    error!("print custom print: {:?}", custom_print);
                    let parser = Punctuated::<Pat, Token![,]>::parse_terminated; //parse_separated_nonempty;
                    let attrs = handle_result!(custom_print.parse_args_with(parser));
                    let length = attrs.len();
                    error!("print attrs: {:?}", attrs);
                    error!("print length: {:?}", length);
                    let attrs2 = attrs.clone();
                    let callsite_span = Span::call_site();
                    let mut attrs_iter = attrs.into_iter();
                    let first_arg = if let Some(text) = attrs_iter.next(){
                        let span = text.span();
                        error!("text node: {:?}", text);
                        match text {
                            Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Str(lit_str) }) }) => {
                                let value = lit_str.value();
                                error!("value of text node: {}", value);
                                let count = value.matches("{}").count();
                                error!("count of {{}} in text node: {}", count);
                                if count != length-1{
                                    return syn::Error::new(
                                        span,
                                        "number of arguments and number of {} do not match",
                                    )
                                    .to_compile_error().into_token_stream();
                                }
                                quote_spanned! {callsite_span=> #value;}
                            },
                            _ => return syn::Error::new(
                                span,
                                "first argument of custom print must be a string literal",
                            )
                            .to_compile_error().into_token_stream(),
                        }
                    }else {
                        return syn::Error::new(
                            attr.span(),
                            "print_counterexample expects at least one argument for struct",
                        )
                        .to_compile_error().into_token_stream();
                    };

            
            let args = attrs_iter.map(|pat | {
                match pat {
                    Pat::Ident(pat_ident) => {
                        quote_spanned! {callsite_span=> #pat_ident; }
                    },
                    Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Int(lit_int)})}) => {
                        quote_spanned! {callsite_span=> #lit_int; }
                    },
                    _ => {error!("variable node {:?}", pat);
                        syn::Error::new(
                        pat.span(),
                        "argument must be a name or an integer",
                    )
                    .to_compile_error().into_token_stream()},
                }
            }).collect::<TokenStream>();

            error!("print args: {}", args);
            error!("print args: {:?}", args);
            let enum_name = item_enum.ident.clone();
            let variant_name = variant.ident.clone();
            let mut rewriter = rewriter::AstRewriter::new();
            let spec_id = rewriter.generate_spec_id();
            let spec_id_str = spec_id.to_string();
            let item_span = variant.ident.span();
            let item_name = syn::Ident::new(
                &format!("prusti_print_counterexample_variant_{}_{}", variant.ident, spec_id),
                item_span,
            );
            let annotation = variant_name.to_string();
                    match variant.fields{
                        Fields::Named(fields_named) => {
                            let mut args2: Punctuated<Pat, Token![,]> = attrs2.into_iter().skip(1).unique().collect::<Punctuated<Pat, Token![,]>>();//TODO skip duplicate
                            if !args2.empty_or_trailing(){
                                args2.push_punct(<syn::Token![,]>::default());
                            }
                            let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                                #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                                #[prusti::spec_only]
                                #[prusti::counterexample_print  = #annotation]
                                #[prusti::spec_id = #spec_id_str]
                                fn #item_name(self) {
                                    if let #enum_name::#variant_name{#args2 ..} = self{
                                        #first_arg
                                        #args
                                    }
                                }
                            };
                            spec_items.push(spec_item);
                        },
                        Fields::Unnamed(fields_unnamed) => {
                            let args2: Punctuated<Pat, Token![,]> = attrs2.into_iter().skip(1).unique().collect::<Punctuated<Pat, Token![,]>>();//TODO skip duplicate
                            
                            //check if all args are possible
                            for arg in &args2{
                                if let Pat::Lit(PatLit { attrs: _, expr: box Expr::Lit(ExprLit { attrs: _, lit: Lit::Int(lit_int)})}) = arg{
                                    let value:u32 = lit_int.base10_parse().ok().unwrap(); //TODO find a better solution //can only be positive //why does handle_resutl not work
                                    error!("print value: {}", value);
                                    if value >= fields_unnamed.unnamed.len() as u32 {
                                        return syn::Error::new(
                                            arg.span(),
                                            format!("variant `{}::{}` does not have a field named {}", item_enum.ident, variant.ident, value),
                                        )
                                        .to_compile_error().into_token_stream();
                                    }
                                } else {
                                    return syn::Error::new(
                                        arg.span(),
                                        format!("variant `{}::{}` needs integer as arguments", item_enum.ident, variant.ident),
                                    )
                                    .to_compile_error().into_token_stream();
                                }
                            }
                            
                            let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                                #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                                #[prusti::spec_only]
                                #[prusti::counterexample_print = #annotation]
                                #[prusti::spec_id = #spec_id_str]
                                fn #item_name(self) {
                                    if let #enum_name::#variant_name(..) = self{
                                        #first_arg
                                        #args
                                    }
                                }
                            };
                            spec_items.push(spec_item);
                        },
                        Fields::Unit => {
                            if length == 1{
                                let spec_item: syn::ItemFn = parse_quote_spanned! {item_span=>
                                    #[allow(unused_must_use, unused_parens, unused_variables, dead_code, non_snake_case, irrefutable_let_patterns)]
                                    #[prusti::spec_only]
                                    #[prusti::counterexample_print = #annotation]
                                    #[prusti::spec_id = #spec_id_str]
                                    fn #item_name(self) {
                                        if let #enum_name::#variant_name = self{
                                            #first_arg
                                        }
                                    }
                                };
                                spec_items.push(spec_item);
                            } else {
                                return syn::Error::new(
                                    attr.span(),
                                    format!("print_counterexample expects exactly one argument for variant `{}::{}`", item_enum.ident, variant.ident),
                                )
                                .to_compile_error().into_token_stream();
                            }
                        },
                    }
                } else {
                    error!("no custom print found");
                }
            }
            error!("print new function: {:?}", spec_items);

            let mut spec_item = TokenStream::new(); //TODO change this
            for x in spec_items{
                x.to_tokens(&mut spec_item);
            }

            
            let item_span = item_enum2.span();
            let generics = &item_enum.generics;
            let generics_idents = generics
                .params
                .iter()
                .filter_map(|generic_param| match generic_param {
                    syn::GenericParam::Type(type_param) => Some(type_param.ident.clone()),
                    _ => None,
                })
                .collect::<syn::punctuated::Punctuated<_, syn::Token![,]>>();
            // TODO: similarly to extern_specs, don't generate an actual impl
            let typ = item_enum.ident;
            let item_impl: syn::ItemImpl = parse_quote_spanned! {item_span=>
                impl #generics #typ <#generics_idents> {
                    #spec_item
                }
            };
            let tmp = quote_spanned! { item_span =>
                #item_enum2
                #item_impl
            };
            error!("print impl: {}", tmp);
            tmp



            /*
            impl Z {
                #[prusti::spec_only]
                fn print_item_f(self){
                    match self{
                        Z::E{h, i, ..} => {"text {} {}"; h; i;}, //namedfield
                        Z::F(..) => {"text {} {}"; 1; 0;}, //check is numeric //unnamed field
                        _ => {"text";}, //unit field
                    };
                }
            }
            
            
            */
            
            /*

            
            let implementations = variants.iter().map(|variant| {
                
                if let Some(print) = variant.attrs.iter().find( |attr| format!("{}", attr.path.get_ident()) == "print_counterexample");
                let parser = Punctuated::<Pat, Token![,]>::parse_terminated; //parse_separated_nonempty;
                let attrs = handle_result!(parser.parse(print.clone().into()));
                let length = attrs.len();
                    
                
                
                
                
                variant.to_token_stream()}).collect::<Vec<TokenStream>>();
            error!("print implementations: {:?}", implementations);
            //let parsed = handle_result!(syn::parse2(implementations.into_iter().next().unwrap())); //.map(| imple| handle_result!(syn::parse2(imple)));
            //error!("print items: {:?}", parsed);*/
        }
        
        
        _ => syn::Error::new(
            attr.span(),
            "Only structs and enums can be attributed with a custom counterexample print",
        )
        .to_compile_error(),
    };
    //let mut result = TokenStream::new();
    //let item2: syn::Item = handle_result!(syn::parse2(tokens));
    //item2.to_tokens(&mut result);
    //spec_item.to_tokens(&mut result);
    spec_item
    //result.clone()
}
