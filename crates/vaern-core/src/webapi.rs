//! Native halves of the Web APIs defined in the JavaScript prelude.
//!
//! Text encoding is done here rather than in JavaScript so that it follows
//! the WHATWG Encoding Standard exactly: invalid UTF-8 decodes to U+FFFD by
//! maximal subpart, and lone surrogates encode as U+FFFD. The prelude keeps
//! the stateful parts (streaming, BOM handling) and calls these helpers for
//! the byte work.
//!
//! URL parsing and serialisation use the `url` crate, the same parser the
//! fetch client resolves request URLs with, so `new URL(x).href` and the URL
//! `fetch(x)` connects to cannot disagree. Its `quirks` module implements the
//! WHATWG getters and setters; `form_urlencoded` implements URLSearchParams.

use crate::error::{Result, VaernError};
use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, TypedArray};
use url::{Url, form_urlencoded, quirks};

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

    let parse = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         input: String,
         base: Opt<String>|
         -> rquickjs::Result<Option<Object<'js>>> {
            let base = match base.0 {
                Some(base) => match Url::parse(&base) {
                    Ok(base) => Some(base),
                    Err(_) => return Ok(None),
                },
                None => None,
            };
            match Url::options().base_url(base.as_ref()).parse(&input) {
                Ok(url) => url_parts(&ctx, &url).map(Some),
                Err(_) => Ok(None),
            }
        },
    );
    set(globals, "__vaern_url_parse", parse)?;

    // Applies one WHATWG setter. An invalid value for a component other than
    // `href` leaves the URL unchanged, as the standard requires.
    let update = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>,
         href: String,
         part: String,
         value: String|
         -> rquickjs::Result<Option<Object<'js>>> {
            let Ok(mut url) = Url::parse(&href) else {
                return Ok(None);
            };
            let applied = match part.as_str() {
                "href" => quirks::set_href(&mut url, &value).is_ok(),
                "protocol" => quirks::set_protocol(&mut url, &value).is_ok(),
                "username" => quirks::set_username(&mut url, &value).is_ok(),
                "password" => quirks::set_password(&mut url, &value).is_ok(),
                "host" => quirks::set_host(&mut url, &value).is_ok(),
                "hostname" => quirks::set_hostname(&mut url, &value).is_ok(),
                "port" => quirks::set_port(&mut url, &value).is_ok(),
                "pathname" => {
                    quirks::set_pathname(&mut url, &value);
                    true
                }
                "search" => {
                    quirks::set_search(&mut url, &value);
                    true
                }
                "hash" => {
                    quirks::set_hash(&mut url, &value);
                    true
                }
                other => {
                    return Err(rquickjs::Exception::throw_type(
                        &ctx,
                        &format!("URL: '{other}' is not a settable component"),
                    ));
                }
            };
            if !applied && part == "href" {
                return Ok(None);
            }
            url_parts(&ctx, &url).map(Some)
        },
    );
    set(globals, "__vaern_url_set", update)?;

    let form_parse = Function::new(ctx.clone(), |query: String| -> Vec<Vec<String>> {
        form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| vec![k.into_owned(), v.into_owned()])
            .collect()
    });
    set(globals, "__vaern_form_parse", form_parse)?;

    let form_serialize = Function::new(ctx.clone(), |pairs: Vec<Vec<String>>| -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        for pair in &pairs {
            if let [name, value] = pair.as_slice() {
                out.append_pair(name, value);
            }
        }
        out.finish()
    });
    set(globals, "__vaern_form_serialize", form_serialize)?;
    Ok(())
}

/// The WHATWG components of `url`, as the prelude's `URL` reads them.
fn url_parts<'js>(ctx: &Ctx<'js>, url: &Url) -> rquickjs::Result<Object<'js>> {
    let parts = Object::new(ctx.clone())?;
    parts.set("href", quirks::href(url))?;
    parts.set("origin", quirks::origin(url))?;
    parts.set("protocol", quirks::protocol(url))?;
    parts.set("username", quirks::username(url))?;
    parts.set("password", quirks::password(url))?;
    parts.set("host", quirks::host(url))?;
    parts.set("hostname", quirks::hostname(url))?;
    parts.set("port", quirks::port(url))?;
    parts.set("pathname", quirks::pathname(url))?;
    parts.set("search", quirks::search(url))?;
    parts.set("hash", quirks::hash(url))?;
    Ok(parts)
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
