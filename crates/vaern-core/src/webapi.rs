//! Native halves of the Web APIs defined in the JavaScript prelude.
//!
//! Text encoding is done here rather than in JavaScript so that it follows
//! the WHATWG Encoding Standard exactly: invalid UTF-8 decodes to U+FFFD by
//! maximal subpart, and lone surrogates encode as U+FFFD. The prelude keeps
//! the stateful parts (streaming, BOM handling) and calls these helpers for
//! the byte work.

use crate::error::{Result, VaernError};
use rquickjs::{Ctx, Function, Object, TypedArray};

/// Registers the native helpers on the global object.
pub fn register<'js>(ctx: &Ctx<'js>, globals: &Object<'js>) -> Result<()> {
    // rquickjs exposes typed-array memory only through `unsafe` accessors, so
    // the prelude passes bytes as a Latin-1 string, one code unit per byte.
    let decode = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, latin1: String, fatal: bool| -> rquickjs::Result<String> {
            let mut bytes = Vec::with_capacity(latin1.len());
            for ch in latin1.chars() {
                match u8::try_from(u32::from(ch)) {
                    Ok(byte) => bytes.push(byte),
                    Err(_) => {
                        return Err(rquickjs::Exception::throw_type(
                            &ctx,
                            "TextDecoder.decode: byte string holds a value above 0xFF",
                        ));
                    }
                }
            }
            if fatal {
                String::from_utf8(bytes).map_err(|err| {
                    rquickjs::Exception::throw_type(
                        &ctx,
                        &format!(
                            "TextDecoder.decode: invalid UTF-8 at byte {}",
                            err.utf8_error().valid_up_to()
                        ),
                    )
                })
            } else {
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
        },
    );
    set(globals, "__vaern_utf8_decode", decode)?;

    // The prelude passes a well-formed string (`toWellFormed` replaces lone
    // surrogates with U+FFFD, the USVString conversion the standard requires);
    // rquickjs refuses to convert a string that still holds one.
    let encode = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, text: String| -> rquickjs::Result<TypedArray<'js, u8>> {
            TypedArray::new(ctx, text.into_bytes())
        },
    );
    set(globals, "__vaern_utf8_encode", encode)?;
    Ok(())
}

fn set<'js>(
    globals: &Object<'js>,
    name: &str,
    function: rquickjs::Result<Function<'js>>,
) -> Result<()> {
    let function = function.map_err(|err| VaernError::EngineInit {
        reason: format!("failed to create {name}: {err}"),
    })?;
    globals
        .set(name, function)
        .map_err(|err| VaernError::EngineInit {
            reason: format!("failed to register {name}: {err}"),
        })
}
