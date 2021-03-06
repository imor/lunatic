use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Pat, PatType, Path, Type, TypeReference};

use super::arg_error;

/// Takes the input arguments part of the host function's signature and returns wrappers around higher
/// level types to make them compatible with WASM guest functions, according to WASI conventions.
///
/// There are 3 parts to this transformation (the return values):
/// 1. The input arguments of the WASM guest function.
/// 2. Code that maps the WASM guest input arguments to the provided host function arguments.
/// 3. List of arguments passed to the host function.
///
/// The following rules are followed when doing the transformation:
/// 1. **i32, i64, f32 and f64** (WASM guest compatible types) are just forwarded to the host function.
/// 2. **&str** is split on the guest in two arguments, a pointer to the string and its length.
/// 3. **&mut [u8]** is split on the guest in two arguments, a pointer to the u8 slice and its length.
/// 4. **&[std::io::IoSlice<'_>]** is split on the guest in two arguments, a pointer to a slice containing WASI
///    ciovec structs and its length.
/// 5. **&mut [IoSliceMut<'_>]** is split on the guest in two arguments, a pointer to a slice containing WASI
///    iovec structs and its length.
/// 6. **Custom types** need to implement uptown_funk::FromWasmI32 and are created from a **i32** wasm type.
/// 7. All other patterns will result in a compilation error.
pub fn transform(
    pat_type: &PatType,
) -> Result<(TokenStream2, TokenStream2, TokenStream2), TokenStream> {
    let argument_name = match &*pat_type.pat {
        Pat::Ident(pat_ident) => {
            if pat_ident.by_ref.is_some() || pat_ident.mutability.is_some() {
                return Err(arg_error(&pat_type.pat));
            };
            &pat_ident.ident
        }
        _ => return Err(arg_error(&pat_type.pat)),
    };

    let argument_transformation = match &*pat_type.ty {
        Type::Path(type_path) => transform_path(&type_path.path),
        Type::Reference(type_ref) => transform_reference(&type_ref),
        _ => return Err(arg_error(&pat_type.ty)),
    };

    let pat_type_ty = &*pat_type.ty;
    match argument_transformation {
        // i32, i64, ...
        Transformation::None => {
            let input_argument = quote! { #pat_type };
            let host_call_argument = quote! { #argument_name };
            Ok((input_argument, quote! {}, host_call_argument))
        }
        // CustomStruct, CustomEnum, ...
        Transformation::CustomType => {
            let input_argument = quote! { #argument_name: i32 };
            let transformation = quote! {
                let #argument_name = <#pat_type_ty as uptown_funk::FromWasmI32>::from_i32(
                    state_wrapper.state(),
                    state_wrapper.instance_environment(),
                    #argument_name
                )?;
            };
            let host_call_argument = quote! { #argument_name };
            Ok((input_argument, transformation, host_call_argument))
        }
        // &CustomStruct, &CustomEnum, ...
        Transformation::RefCustomType => {
            let pat_type_ty_without_ref = match pat_type_ty {
                Type::Reference(type_ref) => &type_ref.elem,
                _ => return Err(arg_error(pat_type_ty)),
            };
            let input_argument = quote! { #argument_name: i32 };
            let transformation = quote! {
                let #argument_name = <#pat_type_ty_without_ref as uptown_funk::FromWasmI32Borrowed>::from_i32_borrowed(
                    state_wrapper.state(),
                    state_wrapper.instance_environment(),
                    #argument_name
                )?;
            };
            let host_call_argument = quote! { #argument_name };
            Ok((input_argument, transformation, host_call_argument))
        }
        // &str
        Transformation::RefStr => {
            let varname_ptr = format_ident!("{}_ptr_", argument_name);
            let varname_len = format_ident!("{}_len_", argument_name);
            let input_argument = quote! { #varname_ptr: i32, #varname_len: i32 };
            let transformation = quote! {
                let #argument_name = {
                    let slice = state_wrapper.wasm_memory().get(
                        #varname_ptr as usize..(#varname_ptr + #varname_len) as usize);
                    let slice = uptown_funk::Trap::try_option(slice)?;
                    let string = std::str::from_utf8(slice);
                    uptown_funk::Trap::try_result(string)?
                };
            };
            let host_call_argument = quote! { #argument_name };
            Ok((input_argument, transformation, host_call_argument))
        }
        // &mut [u8]
        Transformation::RefMutSlice => {
            let varname_ptr = format_ident!("{}_ptr_", argument_name);
            let varname_len = format_ident!("{}_len_", argument_name);
            let input_argument = quote! { #varname_ptr: i32, #varname_len: i32 };
            let transformation = quote! {
                let #argument_name = {
                    let slice = state_wrapper.wasm_memory().get_mut(
                        #varname_ptr as usize..(#varname_ptr + #varname_len) as usize);
                    uptown_funk::Trap::try_option(slice)?
                };
            };
            let host_call_argument = quote! { #argument_name };
            Ok((input_argument, transformation, host_call_argument))
        }
        // &[std::io::IoSlice]
        Transformation::RefSliceIoSlices => {
            let varname_ptr = format_ident!("{}_ptr_", argument_name);
            let varname_len = format_ident!("{}_len_", argument_name);
            let input_argument = quote! { #varname_ptr: i32, #varname_len: i32 };
            let transformation = quote! {
                let #argument_name = {
                    let slice = state_wrapper.wasm_memory().get(
                        #varname_ptr as usize..(#varname_ptr + #varname_len) as usize);
                    let slice = uptown_funk::Trap::try_option(slice)?;
                    let io_slices: &[uptown_funk::IoVecT] = unsafe { std::mem::transmute(slice) };
                    // If we only need 4 or less slices, don't allocate memory.
                    let mut vec_of_io_slices = uptown_funk::SmallVec::<[std::io::IoSlice; 4]>::with_capacity(io_slices.len());
                    for io_vec_t in io_slices.into_iter() {
                        let io_slice = state_wrapper.wasm_memory().get(
                            io_vec_t.ptr as usize..(io_vec_t.ptr + io_vec_t.len) as usize);
                        let io_slice = uptown_funk::Trap::try_option(io_slice)?;
                        let io_slice = std::io::IoSlice::new(io_slice);
                        vec_of_io_slices.push(io_slice);
                    }
                    vec_of_io_slices
                };
            };
            let host_call_argument = quote! { #argument_name.as_slice() };
            Ok((input_argument, transformation, host_call_argument))
        }
        // &mut [std::io::IoSliceMut]
        Transformation::RefMutSliceIoSlicesMut => {
            let varname_ptr = format_ident!("{}_ptr_", argument_name);
            let varname_len = format_ident!("{}_len_", argument_name);
            let input_argument = quote! { #varname_ptr: i32, #varname_len: i32 };
            let transformation = quote! {
                let mut #argument_name = {
                    let slice = state_wrapper.wasm_memory().get_mut(
                        #varname_ptr as usize..(#varname_ptr + #varname_len) as usize);
                    let slice = uptown_funk::Trap::try_option(slice)?;
                    let io_slices: &mut [uptown_funk::IoVecT] = unsafe { std::mem::transmute(slice) };
                    // If we only need 4 or less slices, don't allocate memory.
                    let mut vec_of_io_slices = uptown_funk::SmallVec::<[std::io::IoSliceMut; 4]>::with_capacity(io_slices.len());
                    for io_vec_t in io_slices.into_iter() {
                        let io_slice = state_wrapper.wasm_memory().get_mut(
                            io_vec_t.ptr as usize..(io_vec_t.ptr + io_vec_t.len) as usize);
                        let io_slice = uptown_funk::Trap::try_option(io_slice)?;
                        let io_slice_mut = std::io::IoSliceMut::new(io_slice);
                        vec_of_io_slices.push(io_slice_mut);
                    }
                    vec_of_io_slices
                };
            };
            let host_call_argument = quote! { #argument_name.as_mut_slice() };
            Ok((input_argument, transformation, host_call_argument))
        }
        Transformation::Unsupported => Err(arg_error(&pat_type.ty)),
    }
}

// Transformation for path types i32, CustomType, ...
fn transform_path(path: &Path) -> Transformation {
    if let Some(ident) = path.get_ident() {
        // i32, i64, ...
        if ident == "i32" || ident == "i64" || ident == "f32" || ident == "f64" {
            return Transformation::None;
        } else {
            return Transformation::CustomType;
        }
    }

    Transformation::Unsupported
}

// Transformation for reference types &i32, &str, &mut [u8], ...
fn transform_reference(reference: &TypeReference) -> Transformation {
    if reference.mutability.is_some() {
        return match &*reference.elem {
            Type::Slice(type_slice) => match &*type_slice.elem {
                Type::Path(type_path) => {
                    if let Some(last_segment) = type_path.path.segments.last() {
                        // &mut [std::io::IoSliceMut]
                        if last_segment.ident == "IoSliceMut" {
                            Transformation::RefMutSliceIoSlicesMut
                        // &mut [u8]
                        } else if last_segment.ident == "u8" {
                            Transformation::RefMutSlice
                        } else {
                            Transformation::Unsupported
                        }
                    } else {
                        Transformation::Unsupported
                    }
                }
                _ => Transformation::Unsupported,
            },
            _ => Transformation::Unsupported,
        };
    }

    match &*reference.elem {
        Type::Path(type_path) => {
            if let Some(ident) = type_path.path.get_ident() {
                // &str
                if ident == "str" {
                    return Transformation::RefStr;
                // Everything else is considered a &CustomType
                } else {
                    return Transformation::RefCustomType;
                }
            }
            Transformation::Unsupported
        }
        Type::Slice(type_slice) => match &*type_slice.elem {
            Type::Path(type_path) => {
                if let Some(last_segment) = type_path.path.segments.last() {
                    // &[std::io::IoSlice]
                    if last_segment.ident == "IoSlice" {
                        Transformation::RefSliceIoSlices
                    } else {
                        Transformation::Unsupported
                    }
                } else {
                    Transformation::Unsupported
                }
            }
            _ => Transformation::Unsupported,
        },
        _ => Transformation::Unsupported,
    }
}

enum Transformation {
    None,
    CustomType,
    RefCustomType,
    RefStr,
    RefMutSlice,
    RefSliceIoSlices,
    RefMutSliceIoSlicesMut,
    Unsupported,
}
