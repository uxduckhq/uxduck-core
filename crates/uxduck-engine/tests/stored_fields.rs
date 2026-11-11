//! Every later build reads the checkpoint and the event log. A field that a
//! later build requires is missing from everything an earlier build wrote,
//! and then the daemon cannot start: 0.8.1-beta.2 added `recoveryPoints` to
//! a camera's counters and could not read 0.8.0's files (FB-58 follow-up).
//!
//! So this test reads the source of every type stored in them, from `State`
//! and `Event` down, and lists the fields a stored value must have. If a
//! required field is not in `stored-required-fields.txt`, the test fails:
//! give the field `#[serde(default)]`. Only a field of a type that no release
//! has stored yet may be added to the list.
//! `UXDUCK_BLESS_STORED_FIELDS=1` rewrites the list.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::struct_excessive_bools,
    reason = "a test reading source files"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::meta::ParseNestedMeta;
use syn::punctuated::Punctuated;

const CRATES: &[&str] = &["uxduck-core", "uxduck-engine"];
const ROOTS: &[&str] = &["State", "Event"];
const LIST: &str = "tests/stored-required-fields.txt";
const HEADER: &str = "\
# Fields the checkpoint and event log cannot be read without, checked by
# stored_fields.rs. Everything released builds stored has these, so they
# stay. Never add a field to a type a release has stored: give it
# #[serde(default)] instead, or the next build cannot read existing data.
";

#[test]
fn every_field_a_stored_value_must_have_was_there_from_the_start() {
    let defs = definitions();
    let stored = reachable(&defs);
    assert!(
        ROOTS
            .iter()
            .chain(&["VideoCounters"])
            .all(|name| stored.contains(*name)),
        "the scan did not find the stored types: {stored:?}"
    );
    let required: BTreeSet<String> = stored
        .iter()
        .flat_map(|name| &defs[name])
        .flat_map(required_fields)
        .collect();

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(LIST);
    if std::env::var_os("UXDUCK_BLESS_STORED_FIELDS").is_some() {
        let mut text = HEADER.to_owned();
        for field in &required {
            text.push_str(field);
            text.push('\n');
        }
        std::fs::write(&path, text).unwrap();
        return;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with UXDUCK_BLESS_STORED_FIELDS=1",
            path.display()
        )
    });
    let listed: BTreeSet<String> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();

    let added: Vec<&str> = required.difference(&listed).map(String::as_str).collect();
    assert!(
        added.is_empty(),
        "These stored fields are required, but the checkpoints and event logs that \
         released builds wrote may not have them, so the daemon would not start:\n  {}\n\
         Give each #[serde(default)] (on the struct, when it derives Default). If the \
         type has never been stored by a release, add them to {LIST} instead.",
        added.join("\n  ")
    );
    let gone: Vec<&str> = listed.difference(&required).map(String::as_str).collect();
    assert!(
        gone.is_empty(),
        "No longer required or stored; remove them from {LIST}:\n  {}",
        gone.join("\n  ")
    );
}

/// Every struct and enum that derives `Deserialize`, and every type alias,
/// by name. Names shared across modules keep every definition.
fn definitions() -> BTreeMap<String, Vec<syn::Item>> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for name in CRATES {
        rust_files(&crates.join(name).join("src"), &mut files);
    }
    let mut defs = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let parsed = syn::parse_file(&text)
            .unwrap_or_else(|e| panic!("{} does not parse: {e}", file.display()));
        collect(&parsed.items, &mut defs);
    }
    defs
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn collect(items: &[syn::Item], defs: &mut BTreeMap<String, Vec<syn::Item>>) {
    for item in items {
        let name = match item {
            syn::Item::Struct(s) if derives_deserialize(&s.attrs) => &s.ident,
            syn::Item::Enum(e) if derives_deserialize(&e.attrs) => &e.ident,
            syn::Item::Type(t) => &t.ident,
            syn::Item::Mod(m) if !is_cfg_test(&m.attrs) => {
                if let Some((_, items)) = &m.content {
                    collect(items, defs);
                }
                continue;
            }
            _ => continue,
        };
        defs.entry(name.to_string()).or_default().push(item.clone());
    }
}

fn derives_deserialize(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .filter(|a| a.path().is_ident("derive"))
        .any(|a| {
            a.parse_args_with(Punctuated::<syn::Path, syn::Token![,]>::parse_terminated)
                .is_ok_and(|paths| {
                    paths
                        .iter()
                        .any(|p| p.segments.last().is_some_and(|s| s.ident == "Deserialize"))
                })
        })
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && a.parse_args::<syn::Ident>().is_ok_and(|i| i == "test")
    })
}

/// What `#[serde(...)]` says about reading a container, variant or field.
#[derive(Default)]
struct Serde {
    default: bool,
    skip: bool,
    flatten: bool,
    custom: bool,
    from: Option<syn::Type>,
}

fn serde(attrs: &[syn::Attribute]) -> Serde {
    let mut serde = Serde::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .map(ToString::to_string)
                .unwrap_or_default();
            match name.as_str() {
                "default" => serde.default = true,
                "skip" | "skip_deserializing" => serde.skip = true,
                "flatten" => serde.flatten = true,
                "deserialize_with" | "with" => serde.custom = true,
                "from" | "try_from" => {
                    let ty: syn::LitStr = meta.value()?.parse()?;
                    serde.from = Some(ty.parse()?);
                    return Ok(());
                }
                _ => {}
            }
            skip_value(&meta)
        })
        .unwrap_or_else(|e| panic!("#[serde] does not parse: {e}"));
    }
    serde
}

fn skip_value(meta: &ParseNestedMeta) -> syn::Result<()> {
    if meta.input.peek(syn::Token![=]) {
        meta.value()?.parse::<syn::Expr>()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|inner| skip_value(&inner))?;
    }
    Ok(())
}

/// The stored types: the roots and every type their read fields name.
fn reachable(defs: &BTreeMap<String, Vec<syn::Item>>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<String> = ROOTS.iter().map(|r| (*r).to_owned()).collect();
    while let Some(name) = queue.pop() {
        let Some(items) = defs.get(&name) else {
            continue;
        };
        if !seen.insert(name) {
            continue;
        }
        let mut types = Vec::new();
        for item in items {
            match item {
                syn::Item::Struct(s) => {
                    types.extend(serde(&s.attrs).from);
                    types.extend(read_fields(&s.fields).map(|f| f.ty.clone()));
                }
                syn::Item::Enum(e) => {
                    types.extend(serde(&e.attrs).from);
                    for variant in e.variants.iter().filter(|v| !serde(&v.attrs).skip) {
                        types.extend(read_fields(&variant.fields).map(|f| f.ty.clone()));
                    }
                }
                syn::Item::Type(t) => types.push((*t.ty).clone()),
                _ => {}
            }
        }
        for ty in &types {
            type_names(ty, &mut queue);
        }
    }
    seen
}

fn read_fields(fields: &syn::Fields) -> impl Iterator<Item = &syn::Field> {
    fields.iter().filter(|f| !serde(&f.attrs).skip)
}

fn type_names(ty: &syn::Type, out: &mut Vec<String>) {
    match ty {
        syn::Type::Path(p) => {
            if let Some(q) = &p.qself {
                type_names(&q.ty, out);
            }
            for segment in &p.path.segments {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    for arg in &args.args {
                        if let syn::GenericArgument::Type(t) = arg {
                            type_names(t, out);
                        }
                    }
                }
            }
            if let Some(last) = p.path.segments.last() {
                out.push(last.ident.to_string());
            }
        }
        syn::Type::Array(a) => type_names(&a.elem, out),
        syn::Type::Slice(s) => type_names(&s.elem, out),
        syn::Type::Reference(r) => type_names(&r.elem, out),
        syn::Type::Paren(p) => type_names(&p.elem, out),
        syn::Type::Group(g) => type_names(&g.elem, out),
        syn::Type::Tuple(t) => t.elems.iter().for_each(|e| type_names(e, out)),
        _ => {}
    }
}

/// `Type.field`, or `Enum::Variant.field`, for each named field a stored
/// value cannot be read without.
fn required_fields(item: &syn::Item) -> Vec<String> {
    let (name, container, members): (_, _, Vec<(String, &syn::Fields)>) = match item {
        syn::Item::Struct(s) => (&s.ident, serde(&s.attrs), vec![(String::new(), &s.fields)]),
        syn::Item::Enum(e) => (
            &e.ident,
            serde(&e.attrs),
            e.variants
                .iter()
                .filter(|v| !serde(&v.attrs).skip)
                .map(|v| (format!("::{}", v.ident), &v.fields))
                .collect(),
        ),
        _ => return Vec::new(),
    };
    if container.default || container.from.is_some() {
        return Vec::new();
    }
    let mut required = Vec::new();
    for (variant, fields) in members {
        let syn::Fields::Named(named) = fields else {
            continue;
        };
        for field in &named.named {
            let attrs = serde(&field.attrs);
            let optional = is_option(&field.ty) && !attrs.custom;
            if !(attrs.default || attrs.skip || attrs.flatten || optional) {
                let field = field.ident.as_ref().unwrap();
                required.push(format!("{name}{variant}.{field}"));
            }
        }
    }
    required
}

/// serde reads a missing `Option` as `None`.
fn is_option(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(p) if p.qself.is_none()
        && p.path.segments.last().is_some_and(|s| s.ident == "Option"))
}
